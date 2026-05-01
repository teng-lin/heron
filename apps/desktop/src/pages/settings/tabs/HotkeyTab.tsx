import { useEffect, useMemo, useRef, useState } from "react";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Button } from "../../../components/ui/button";
import { Input } from "../../../components/ui/input";
import { Label } from "../../../components/ui/label";
import { invoke } from "../../../lib/invoke";
import { useSettingsStore } from "../../../store/settings";
import { CustomShortcutsCard } from "../sections/CustomShortcutsCard";
import {
  createHotkeySyncController,
  createHotkeyTestController,
} from "../utils/hotkey-sync";
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
  // Mirror of the saved chord, kept in a ref so the test controller's
  // `getCurrentCombo` always reads the *latest* value rather than a
  // stale closure capture. Issue #212 item 7 — without this, the
  // late-response guard couldn't tell when the user had moved on.
  const currentComboRef = useRef<string>(settings?.record_hotkey ?? "");
  useEffect(() => {
    currentComboRef.current = settings?.record_hotkey ?? "";
    // Changing the saved chord invalidates any in-flight Test result.
    // Without this, a late `heron_check_hotkey` response that the
    // controller drops as stale would leave `conflict` stuck on
    // "checking" — the spinner would never resolve and the Test
    // button would stay disabled. The captureChord handler covers
    // the keyboard path; this useEffect covers external paths
    // (settings load resolving mid-test, another tab editing, etc.).
    setConflict("unknown");
  }, [settings?.record_hotkey]);

  // Pure state-machine controller for the OS-level hotkey registration.
  // The factory captures injectable IPC + a toast sink so the
  // controller can be exercised under `bun test` without React. See
  // `utils/hotkey-sync.ts` for the contract; the controller owns
  // ordering (register-then-unregister), empty-input handling
  // (unregister + clear), and a generation counter to drop stale
  // results from rapid edits.
  const syncController = useMemo(
    () =>
      createHotkeySyncController({
        registerHotkey: (combo) => invoke("heron_register_hotkey", { combo }),
        unregisterHotkey: (combo) =>
          invoke("heron_unregister_hotkey", { combo }),
        onError: (message) =>
          toast.error(`Hotkey registration failed: ${message}`),
      }),
    [],
  );

  // Pure controller for the Test button's `heron_check_hotkey` flow.
  // The controller's generation counter + capture-and-compare keeps a
  // late response for an abandoned chord from surfacing a stale
  // conflict toast. Issue #212 item 7.
  const testController = useMemo(
    () =>
      createHotkeyTestController({
        checkHotkey: (combo) => invoke("heron_check_hotkey", { combo }),
        onResult: (free) => setConflict(free ? "free" : "conflict"),
        onError: (message) => {
          toast.error(`Could not test hotkey: ${message}`);
          setConflict("unknown");
        },
        getCurrentCombo: () => currentComboRef.current,
      }),
    [],
  );

  // On mount + on every saved-chord change, reconcile the OS
  // registration to match. The autosave path persists each edit; the
  // controller short-circuits when the next combo already matches
  // what's registered.
  useEffect(() => {
    if (settings === null) return;
    void syncController.sync(settings.record_hotkey);
  }, [settings?.record_hotkey, syncController]);

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
    await testController.runCheck(settings.record_hotkey);
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
