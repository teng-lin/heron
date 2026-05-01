import { useCallback, useEffect, useState } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import { CheckCircle2, Circle, Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Button } from "../../../components/ui/button";
import { Input } from "../../../components/ui/input";
import { Label } from "../../../components/ui/label";
import { invoke, type KeychainAccount } from "../../../lib/invoke";

interface KeychainKeyFieldProps {
  /** Wire-format account label; matches a `KeychainAccount` Rust variant. */
  account: KeychainAccount;
  /** Visible field label (e.g. "Anthropic API key"). */
  label: string;
  /** Placeholder for the password input inside the modal. */
  placeholder: string;
  /** Caption rendered under the status pill. */
  helpText: string;
}

/**
 * One row in the Summarizer tab for an API-key slot stored in the
 * macOS login Keychain (PR-θ / phase 70).
 *
 * Renders:
 *   - a status pill: "set" (green CheckCircle) / "not set" (gray
 *     Circle), driven by `heron_keychain_has`,
 *   - "Edit" → opens a Radix modal with a password input + Save +
 *     Delete + Cancel,
 *   - "Delete" → asks for confirmation before calling
 *     `heron_keychain_delete`.
 *
 * Crucially, the renderer NEVER asks for the secret value back. The
 * modal owns the cleartext only for the lifetime of one keystroke
 * session and clears it on close (success or cancel). Component state
 * is the only place the secret lives JS-side, and even there it
 * leaves no trail in the React DevTools tree because the input is
 * `type="password"`.
 *
 * The Save/Delete writes are immediate — they bypass the page-level
 * autosave + dirty-flag entirely. Keychain edits are intentionally
 * not coupled to the broader Settings form's save lifecycle: a user
 * who hits Save in the modal expects the secret to land regardless
 * of whether they have other unsaved settings changes.
 */
export function KeychainKeyField({
  account,
  label,
  placeholder,
  helpText,
}: KeychainKeyFieldProps) {
  // `null` = still loading the initial state from the Keychain probe;
  // `boolean` = answer in hand. The "Edit" button stays interactive
  // even while loading so the user isn't blocked on a slow probe.
  const [hasKey, setHasKey] = useState<boolean | null>(null);
  const [editOpen, setEditOpen] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const ok = await invoke("heron_keychain_has", { account });
      setHasKey(ok);
    } catch (err) {
      // Probe failures (Linux dev build, locked keychain, etc.)
      // collapse to "not set" so the UI keeps working — the user can
      // still try Edit, which will surface the real error from the
      // backend if it persists.
      const message = err instanceof Error ? err.message : String(err);
      // eslint-disable-next-line no-console -- diagnostic only; never logs the secret.
      console.warn(`keychain probe failed for ${account}:`, message);
      setHasKey(false);
    }
  }, [account]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-3">
        <div className="space-y-1">
          <Label>{label}</Label>
          <KeychainStatusPill state={hasKey} />
        </div>
        <Button variant="outline" onClick={() => setEditOpen(true)}>
          Edit…
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">{helpText}</p>

      <KeychainEditDialog
        open={editOpen}
        onOpenChange={setEditOpen}
        account={account}
        label={label}
        placeholder={placeholder}
        hasKey={hasKey === true}
        onChanged={() => void refresh()}
      />
    </div>
  );
}

/**
 * Tri-state status pill for a Keychain slot.
 *
 * - `null` → "Checking…" (probe in flight),
 * - `true` → green check + "key set",
 * - `false` → gray circle + "Not set".
 *
 * The pill is decorative — the heavy lifting (Edit / Delete) lives
 * on the buttons next to it.
 */
function KeychainStatusPill({ state }: { state: boolean | null }) {
  if (state === null) {
    return (
      <span className="flex items-center gap-1 text-xs text-muted-foreground">
        <Loader2 className="h-3 w-3 animate-spin" aria-hidden="true" />
        Checking…
      </span>
    );
  }
  if (state) {
    return (
      <span className="flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
        <CheckCircle2 className="h-3.5 w-3.5" aria-hidden="true" />
        Key set
      </span>
    );
  }
  return (
    <span className="flex items-center gap-1 text-xs text-muted-foreground">
      <Circle className="h-3.5 w-3.5" aria-hidden="true" />
      Not set
    </span>
  );
}

interface KeychainEditDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  account: KeychainAccount;
  label: string;
  placeholder: string;
  hasKey: boolean;
  onChanged: () => void;
}

/**
 * Modal that owns the cleartext secret for the duration of one edit.
 *
 * Lifecycle:
 *   1. open → empty input, `confirmingDelete=false`,
 *   2. user types into `secret`,
 *   3. Save → `heron_keychain_set`, toast, close + refresh parent,
 *   4. Delete → flips to confirmation mode; second click confirms,
 *   5. Cancel / Esc / overlay-click → close (drops the secret).
 *
 * The `secret` state is cleared whenever `open` flips to `false` so a
 * re-open starts blank, and so a back-button-style remount can't
 * leave the value lingering in React's reconciliation memory.
 */
function KeychainEditDialog({
  open,
  onOpenChange,
  account,
  label,
  placeholder,
  hasKey,
  onChanged,
}: KeychainEditDialogProps) {
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);

  // Belt-and-suspenders: every time `open` flips, wipe the cleartext
  // + reset the delete-confirmation latch. The `Dialog.Content`
  // unmounts when `open=false`, but Radix portals can keep the
  // subtree alive across rapid toggles, and we'd rather not assume.
  useEffect(() => {
    if (!open) {
      setSecret("");
      setConfirmingDelete(false);
    }
  }, [open]);

  async function handleSave() {
    // Reject empty strings on the JS side so the user gets a clean
    // toast rather than an opaque backend error. (The Rust shim
    // would happily store an empty secret — that's almost certainly
    // not what the user meant.)
    const trimmed = secret.trim();
    if (trimmed === "") {
      toast.error("Enter a key value, or hit Cancel.");
      return;
    }
    setBusy(true);
    try {
      await invoke("heron_keychain_set", { account, secret: trimmed });
      toast.success(`${label} saved.`);
      // Wipe the cleartext from component state before closing so the
      // value can't be observed via React DevTools after the modal
      // animates out.
      setSecret("");
      onChanged();
      onOpenChange(false);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Could not save ${label}: ${message}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleDelete() {
    if (!confirmingDelete) {
      setConfirmingDelete(true);
      return;
    }
    setBusy(true);
    try {
      await invoke("heron_keychain_delete", { account });
      toast.success(`${label} deleted.`);
      setSecret("");
      onChanged();
      onOpenChange(false);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Could not delete ${label}: ${message}`);
    } finally {
      setBusy(false);
      setConfirmingDelete(false);
    }
  }

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (busy) return; // ignore close-during-write to avoid losing the result toast
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
        >
          <Dialog.Title className="text-lg font-semibold">{label}</Dialog.Title>
          <Dialog.Description
            id={`keychain-desc-${account}`}
            className="text-sm text-muted-foreground"
          >
            {hasKey
              ? "A key is already stored. Enter a new value to replace it, or delete the existing entry."
              : "Paste your key. heron stores it in the macOS login Keychain only."}
          </Dialog.Description>

          <div className="space-y-2">
            <Label htmlFor={`keychain-${account}`}>Key</Label>
            <Input
              id={`keychain-${account}`}
              type="password"
              // `new-password` is the broadly-honored value for
              // suppressing password-manager autofill on a field
              // that isn't a real login form. Plain `off` is honored
              // inconsistently on `type="password"` across Chromium /
              // Safari / Firefox.
              autoComplete="new-password"
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              placeholder={placeholder}
              value={secret}
              onChange={(e) => setSecret(e.target.value)}
              disabled={busy}
              // Tie the input to the dialog description so screen
              // readers re-announce the "stored in Keychain" copy
              // when the input takes focus (Radix's auto-wiring on
              // `Dialog.Content` only fires on dialog open, not on
              // subsequent focus events).
              aria-describedby={`keychain-desc-${account}`}
            />
          </div>

          <div className="flex flex-wrap items-center justify-between gap-2 pt-2">
            <div>
              {hasKey && (
                <Button
                  variant="destructive"
                  onClick={() => void handleDelete()}
                  disabled={busy}
                >
                  {confirmingDelete ? "Click again to confirm" : "Delete"}
                </Button>
              )}
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="ghost"
                onClick={() => onOpenChange(false)}
                disabled={busy}
              >
                Cancel
              </Button>
              <Button
                onClick={() => void handleSave()}
                // Use the trimmed length so a whitespace-only input
                // looks "empty" to the button — matches handleSave's
                // own trim() guard and avoids a flicker where the
                // button is enabled but the click toasts "enter a
                // key value" anyway.
                disabled={busy || secret.trim() === ""}
              >
                {busy ? (
                  <>
                    <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
                    Saving
                  </>
                ) : (
                  "Save"
                )}
              </Button>
            </div>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
