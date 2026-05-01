/**
 * Pure state-machine controller for the Settings → Hotkey OS-level
 * registration sync.
 *
 * The `HotkeyTab` React effect was previously inlined in the
 * component. The two failure modes on issue #212 item 6 are easier to
 * pin with a Bun-only state-machine test — there's no jsdom in this
 * workspace (see `components/ActionItemsEditor.test.ts` for the same
 * pattern). The factory takes injectable IPC functions so the
 * controller can be exercised without React or Tauri.
 *
 * ## State the controller owns
 *
 *   - `registeredCombo` — the chord currently registered with the OS.
 *     Mirrors `registeredComboRef` in the previous inlined effect.
 *   - `syncGen` — monotonic counter so concurrent sync runs (rapid
 *     hotkey edits before the previous async register resolves) don't
 *     race each other. Only the latest generation is allowed to
 *     mutate `registeredCombo` or surface a toast.
 *
 * ## Sync ordering (issue #212 item 6)
 *
 * Register the new combo first, only then unregister the old. The
 * previous shape unregistered first — a failed register left the user
 * with no working hotkey. When `next === ""` (user cleared the
 * field), unregister the previous combo and clear `registeredCombo`.
 */

export interface HotkeySyncDeps {
  registerHotkey: (combo: string) => Promise<unknown>;
  unregisterHotkey: (combo: string) => Promise<unknown>;
  /** Optional error sink. Defaults to no-op. */
  onError?: (message: string) => void;
}

export interface HotkeySyncController {
  /**
   * Reconcile the OS registration with `next`. Idempotent: a no-op
   * when `next` already equals the registered combo. Empty `next`
   * unregisters the previously-registered combo.
   */
  sync(next: string): Promise<void>;
  /** Snapshot of the currently-registered combo. `null` before the first sync. */
  getRegisteredCombo(): string | null;
}

export function createHotkeySyncController(
  deps: HotkeySyncDeps,
): HotkeySyncController {
  const onError = deps.onError ?? (() => {});
  let registeredCombo: string | null = null;
  let syncGen = 0;

  return {
    getRegisteredCombo() {
      return registeredCombo;
    },
    async sync(next) {
      // Empty input clears the registration. The previous shape bailed
      // here, leaving the OS hotkey active after the user cleared the
      // field. Issue #212 item 6.
      if (next === "") {
        if (registeredCombo === null) return;
        const myGen = ++syncGen;
        const previous = registeredCombo;
        try {
          await deps.unregisterHotkey(previous);
          if (syncGen !== myGen) return;
          registeredCombo = null;
        } catch (err) {
          if (syncGen !== myGen) return;
          onError(err instanceof Error ? err.message : String(err));
        }
        return;
      }
      if (registeredCombo === next) return;

      const myGen = ++syncGen;
      const previous = registeredCombo;
      try {
        // Register the new combo FIRST, then unregister the old. The
        // previous shape did this in reverse order — a failed register
        // left the user with no working hotkey. Issue #212 item 6.
        await deps.registerHotkey(next);
        if (syncGen !== myGen) return;
        if (previous !== null && previous !== next) {
          try {
            await deps.unregisterHotkey(previous);
          } catch (err) {
            // Unregister failure is not fatal — the new combo is
            // already live. Surface to the caller's error sink so a
            // toast can fire if desired, but don't roll back.
            if (syncGen !== myGen) return;
            onError(err instanceof Error ? err.message : String(err));
          }
        }
        if (syncGen !== myGen) return;
        registeredCombo = next;
      } catch (err) {
        if (syncGen !== myGen) return;
        onError(err instanceof Error ? err.message : String(err));
      }
    },
  };
}
