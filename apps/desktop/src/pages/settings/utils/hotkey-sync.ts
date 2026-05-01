/**
 * Pure state-machine controllers for the Settings → Hotkey tab.
 *
 * The `HotkeyTab` React effects were previously inlined in the
 * component. The failure modes on issue #212 items 6 and 7 are easier
 * to pin with a Bun-only state-machine test — there's no jsdom in
 * this workspace (see `components/ActionItemsEditor.test.ts` for the
 * same pattern). Each factory takes injectable IPC functions so the
 * controllers can be exercised without React or Tauri.
 *
 * ## Sync controller (issue #212 item 6)
 *
 *   - `registeredCombo` — the chord currently registered with the OS.
 *     Mirrors `registeredComboRef` in the previous inlined effect.
 *   - `syncGen` — monotonic counter so concurrent sync runs (rapid
 *     hotkey edits before the previous async register resolves) don't
 *     race each other. Only the latest generation is allowed to
 *     mutate `registeredCombo` or surface a toast.
 *
 * Register the new combo first, only then unregister the old. The
 * previous shape unregistered first — a failed register left the user
 * with no working hotkey. When `next === ""` (user cleared the
 * field), unregister the previous combo and clear `registeredCombo`.
 *
 * ## Test controller (issue #212 item 7)
 *
 *   - `testGen` — monotonic counter so a late `heron_check_hotkey`
 *     response for an abandoned chord doesn't surface a stale
 *     conflict toast.
 *
 * Each `runCheck` capture-and-compares against the caller's current
 * combo (via `getCurrentCombo`). If the user changed the chord
 * between firing the test and the IPC resolving, the result is
 * dropped silently.
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

export interface HotkeyTestDeps {
  checkHotkey: (combo: string) => Promise<boolean>;
  /**
   * Fires when the IPC resolves AND the captured combo still matches
   * the caller's current combo (per `getCurrentCombo`). The argument
   * is `true` when the chord is free, `false` on conflict.
   */
  onResult: (free: boolean) => void;
  /** Optional error sink. Defaults to no-op. */
  onError?: (message: string) => void;
  /**
   * The caller's source of truth for the *current* combo. The
   * controller calls this after `checkHotkey` resolves so a late
   * response for an abandoned chord doesn't surface a stale result.
   */
  getCurrentCombo: () => string;
}

export interface HotkeyTestController {
  /**
   * Run `checkHotkey` against `combo`. Late responses are dropped if
   * the caller's `getCurrentCombo()` no longer matches the combo
   * captured at request time.
   */
  runCheck(combo: string): Promise<void>;
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
        if (syncGen !== myGen) {
          // A newer sync started while we were registering. Roll back
          // our orphan OS-level claim so rapid edits don't leak ghost
          // registrations the user never asked for. The newer sync
          // will register its own combo independently.
          try {
            await deps.unregisterHotkey(next);
          } catch {
            // Best-effort rollback. A stale unregister is ignorable —
            // the next live sync will reconcile the OS state.
          }
          return;
        }
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

export function createHotkeyTestController(
  deps: HotkeyTestDeps,
): HotkeyTestController {
  const onError = deps.onError ?? (() => {});
  let testGen = 0;

  return {
    async runCheck(combo) {
      const myGen = ++testGen;
      const myCombo = combo;
      try {
        const free = await deps.checkHotkey(myCombo);
        // Two guards: (1) a newer test ran while ours was in flight;
        // (2) the user changed the chord without re-running Test.
        // Either way, the result we have is stale. Issue #212 item 7.
        if (testGen !== myGen) return;
        if (deps.getCurrentCombo() !== myCombo) return;
        deps.onResult(free);
      } catch (err) {
        if (testGen !== myGen) return;
        if (deps.getCurrentCombo() !== myCombo) return;
        onError(err instanceof Error ? err.message : String(err));
      }
    },
  };
}
