import { useEffect, useRef, useState } from "react";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Button } from "../../../components/ui/button";
import { Input } from "../../../components/ui/input";
import { Label } from "../../../components/ui/label";
import { invoke } from "../../../lib/invoke";
import { useSettingsStore } from "../../../store/settings";
import { CustomShortcutsCard } from "../sections/CustomShortcutsCard";
import { normalizeKey } from "../utils/keys";

/**
 * Result of `heron_check_hotkey` rendered next to the Test button.
 * Tri-state because a fresh tab visit shouldn't show stale "free" /
 * "conflict" copy from the previous chord.
 */
type ConflictState = "unknown" | "free" | "conflict" | "checking";

export function HotkeyTab() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);
  const [capturing, setCapturing] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const [conflict, setConflict] = useState<ConflictState>("unknown");
  // Track the chord that was *registered with the OS* so a save that
  // changes the chord can call `heron_unregister_hotkey(oldCombo)`
  // before registering the new one. We seed this with `null`; the
  // first effect run picks up whatever was registered at app startup
  // (lib.rs::register_startup_hotkey) — `heron_register_hotkey` is
  // idempotent, so the redundant call is harmless.
  const registeredComboRef = useRef<string | null>(null);
  // Monotonic counter so concurrent sync runs (rapid hotkey edits
  // before the previous async register resolves) don't race each
  // other. See the effect's "generation counter" comment.
  const hotkeySyncGenRef = useRef<number>(0);

  // On mount + on every saved-chord change, sync the OS-level
  // registration to match. The dependency list watches
  // `settings.record_hotkey`: the autosave path persists each edit,
  // and we re-register here whenever the saved combo changes.
  //
  // ## Why a generation counter, not just `cancelled`
  //
  // Rapid hotkey edits (capture-then-edit-again before the first
  // async register resolves) would otherwise race: effect-A's
  // unregister call would observe `registeredComboRef` still pointing
  // at the pre-A combo, and effect-A's late `ref = A` write could
  // overwrite effect-B's `ref = B`. The `gen` counter pins each
  // effect run to a snapshot of the run number; only the latest
  // generation is allowed to mutate the ref or surface a toast.
  useEffect(() => {
    if (settings === null) return;
    const next = settings.record_hotkey;
    if (next === "") return;
    if (registeredComboRef.current === next) return;

    const myGen = ++hotkeySyncGenRef.current;
    let cancelled = false;
    const previousCombo = registeredComboRef.current;

    async function syncRegistration() {
      try {
        if (previousCombo !== null) {
          await invoke("heron_unregister_hotkey", { combo: previousCombo });
        }
        await invoke("heron_register_hotkey", { combo: next });
        if (cancelled || hotkeySyncGenRef.current !== myGen) {
          // A newer sync started (or we unmounted) while ours was in
          // flight. Don't claim ownership of the ref — the newer
          // run's success will handle it.
          return;
        }
        registeredComboRef.current = next;
      } catch (err) {
        if (cancelled || hotkeySyncGenRef.current !== myGen) return;
        const message = err instanceof Error ? err.message : String(err);
        toast.error(`Hotkey registration failed: ${message}`);
      }
    }
    void syncRegistration();
    return () => {
      cancelled = true;
    };
  }, [settings?.record_hotkey]);

  if (settings === null) return null;

  // Capture a keystroke while the input is focused. Records the chord
  // in Tauri's `tauri-plugin-global-shortcut` syntax so the Rust side
  // can register it verbatim. Modifier-only keystrokes are ignored.
  function captureChord(e: React.KeyboardEvent<HTMLInputElement>) {
    if (!capturing) return;
    e.preventDefault();
    if (e.key === "Escape") {
      setCapturing(false);
      inputRef.current?.blur();
      return;
    }
    if (["Meta", "Control", "Alt", "Shift"].includes(e.key)) {
      // Wait for a non-modifier key.
      return;
    }
    const parts: string[] = [];
    if (e.metaKey || e.ctrlKey) parts.push("CmdOrCtrl");
    if (e.altKey) parts.push("Alt");
    if (e.shiftKey) parts.push("Shift");
    parts.push(normalizeKey(e.key));
    update({ record_hotkey: parts.join("+") });
    // Capturing a new chord invalidates the previous Test result.
    setConflict("unknown");
    setCapturing(false);
    inputRef.current?.blur();
  }

  async function runCheck() {
    if (settings === null) return;
    setConflict("checking");
    try {
      const free = await invoke("heron_check_hotkey", {
        combo: settings.record_hotkey,
      });
      setConflict(free ? "free" : "conflict");
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Could not test hotkey: ${message}`);
      setConflict("unknown");
    }
  }

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Hotkey</h2>

      <div className="space-y-2">
        <Label htmlFor="record-hotkey">Record / stop hotkey</Label>
        <div className="flex gap-2">
          <Input
            id="record-hotkey"
            ref={inputRef}
            readOnly
            value={settings.record_hotkey}
            onFocus={() => setCapturing(true)}
            onBlur={() => setCapturing(false)}
            onKeyDown={captureChord}
            placeholder="Click to capture"
            className="font-mono"
          />
          <Button
            variant="outline"
            onClick={() => void runCheck()}
            disabled={settings.record_hotkey === "" || conflict === "checking"}
          >
            {conflict === "checking" ? (
              <>
                <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
                Testing…
              </>
            ) : (
              "Test"
            )}
          </Button>
        </div>
        <p className="text-xs text-muted-foreground">
          {capturing
            ? "Press the chord you'd like — Escape to cancel."
            : "Click the field and press a chord to rebind."}
        </p>
        {conflict === "free" && (
          <p className="text-xs text-emerald-600 dark:text-emerald-400">
            Chord is free — no other app has claimed it.
          </p>
        )}
        {conflict === "conflict" && (
          <p className="text-xs text-destructive">
            Another app already owns this chord. Pick a different one.
          </p>
        )}
        <p className="text-xs text-muted-foreground">
          Triggers Start/Stop Recording from anywhere in macOS.
        </p>
      </div>

      <CustomShortcutsCard />
    </section>
  );
}
