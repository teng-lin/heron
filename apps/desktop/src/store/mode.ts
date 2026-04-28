/**
 * Companion-mode pill state for the TitleBar.
 *
 * `active` mirrors `Settings.active_mode` and persists through
 * `useSettingsStore`. `setActive` writes the new mode to disk
 * immediately rather than relying on the Settings pane's debounced
 * autosave — a user who flips the pill and closes the app within the
 * autosave window must still see their choice on next launch.
 *
 * The store is a thin computed view over `useSettingsStore`, not a
 * second source of truth. Subscribing here is equivalent to
 * `useSettingsStore((s) => s.settings?.active_mode)` but with explicit
 * default-resolution and a write helper that round-trips disk in one
 * call.
 */

import { useSettingsStore } from "./settings";
import type { ActiveMode } from "../lib/invoke";

const DEFAULT_MODE: ActiveMode = "clio";

export function useActiveMode(): ActiveMode {
  return useSettingsStore(
    (state) => state.settings?.active_mode ?? DEFAULT_MODE,
  );
}

/**
 * Persist a mode change. Updates the in-memory snapshot synchronously
 * (so the React tree re-renders with the new pill state immediately)
 * and fires `save()` to flush disk. Resolves once the disk write
 * completes; callers that don't await will still see the UI update
 * but won't notice a write failure.
 */
export async function setActiveMode(mode: ActiveMode): Promise<boolean> {
  const { settings, update, save } = useSettingsStore.getState();
  if (settings === null) {
    return false;
  }
  if (settings.active_mode === mode) {
    return true;
  }
  update({ active_mode: mode });
  return save();
}
