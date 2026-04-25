/**
 * Settings-pane state for `/settings`.
 *
 * Mirrors the `Settings` Rust struct (see `apps/desktop/src-tauri/
 * src/settings.rs`); the field names + serde shape match exactly so
 * `invoke('heron_write_settings', { settings })` round-trips without
 * a translation layer.
 *
 * Lifecycle
 * ---------
 * 1. `load()`  — resolves the platform `settings.json` path via
 *    `heron_default_settings_path`, then reads the file. Missing-file
 *    is a clean no-op (the Rust side returns defaults; we still seed
 *    the store with that). Other read failures are surfaced via
 *    `error` so the form can show a banner without falling back to
 *    silent defaults.
 * 2. `update(patch)` — partial-merge into the in-memory snapshot,
 *    flips `dirty=true`. Pure local — does NOT touch disk.
 * 3. `save()`  — writes the current snapshot via
 *    `heron_write_settings`. Coalesces in-flight calls (a debounce
 *    tick + a Save-button click should not race), and clears
 *    `dirty` only on success so a failed write keeps the UI in a
 *    "you have unsaved changes" state.
 *
 * In-flight coalescing follows the same idea as `store/status.ts`,
 * but uses a held Promise reference rather than a polling loop so
 * concurrent callers resolve exactly when the in-flight write resolves
 * — no spin, no max-wait timeout to babysit.
 */

import { create } from "zustand";

import { invoke, type Settings } from "../lib/invoke";

interface SettingsState {
  /** Latest in-memory snapshot. `null` until `load()` resolves. */
  settings: Settings | null;
  /** Resolved settings.json path (returned by the Rust side). */
  settingsPath: string | null;
  /** True iff the in-memory snapshot has unsaved changes. */
  dirty: boolean;
  /** True while a load is in flight. */
  loading: boolean;
  /** True while a save is in flight. */
  saving: boolean;
  /** Last error message from a failed load/save, or `null`. */
  error: string | null;
  /** Read settings from disk. Idempotent — concurrent calls coalesce. */
  load: () => Promise<void>;
  /** Patch the in-memory snapshot. Marks the store dirty. */
  update: (patch: Partial<Settings>) => void;
  /**
   * Persist the current snapshot. Resolves once the write completes.
   * Concurrent callers piggy-back on the in-flight write rather than
   * queueing a second one. Returns `true` on success, `false` on
   * failure (the error message is also written to `error`).
   */
  save: () => Promise<boolean>;
}

/**
 * Held outside the Zustand state because it's a transient
 * implementation detail of `save()`'s coalescing — including it in
 * `SettingsState` would force every subscriber to re-render whenever
 * the in-flight Promise reference flipped, with no observable benefit.
 */
let inFlightSave: Promise<boolean> | null = null;
let inFlightLoad: Promise<void> | null = null;

export const useSettingsStore = create<SettingsState>((set, get) => ({
  settings: null,
  settingsPath: null,
  dirty: false,
  loading: false,
  saving: false,
  error: null,
  load: async () => {
    if (inFlightLoad !== null) {
      // A concurrent `load()` is in flight — share its result rather
      // than racing it. Without this, a remount-induced second call
      // (StrictMode mounts effects twice in dev) could clobber the
      // first's resolved snapshot with a slower duplicate response.
      return inFlightLoad;
    }
    set({ loading: true, error: null });
    inFlightLoad = (async () => {
      try {
        const settingsPath = await invoke("heron_default_settings_path");
        const settings = await invoke("heron_read_settings", { settingsPath });
        set({
          settings,
          settingsPath,
          dirty: false,
          loading: false,
          error: null,
        });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        set({ loading: false, error: message });
      } finally {
        inFlightLoad = null;
      }
    })();
    return inFlightLoad;
  },
  update: (patch) => {
    const current = get().settings;
    if (current === null) {
      // No snapshot to patch — `update` before `load` resolved. The
      // Settings page disables every input until `settings !== null`,
      // so this branch should not fire in practice; ignoring the call
      // is the safe fallback.
      return;
    }
    set({ settings: { ...current, ...patch }, dirty: true });
  },
  save: async () => {
    if (inFlightSave !== null) {
      // Piggy-back on the in-flight write. The caller gets the same
      // outcome as the original `save()` rather than queueing a
      // second write that would clobber its result.
      return inFlightSave;
    }
    const { settings, settingsPath } = get();
    if (settings === null || settingsPath === null) {
      const message = "Settings not loaded";
      set({ error: message });
      return false;
    }
    // Snapshot the reference we're persisting. If `update()` mutates
    // `settings` while the write is in flight, the post-save `dirty`
    // reset would otherwise erase the user's pending edits from the
    // UI. Reference equality is sufficient because `update()` always
    // assigns a fresh object via spread.
    const savedSnapshot = settings;
    set({ saving: true, error: null });
    inFlightSave = (async () => {
      try {
        await invoke("heron_write_settings", { settingsPath, settings });
        const stillCurrent = get().settings === savedSnapshot;
        set({
          saving: false,
          dirty: stillCurrent ? false : get().dirty,
          error: null,
        });
        return true;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        set({ saving: false, error: message });
        return false;
      } finally {
        inFlightSave = null;
      }
    })();
    return inFlightSave;
  },
}));
