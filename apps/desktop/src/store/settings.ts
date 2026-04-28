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
  /**
   * Read settings from disk only if not already cached. Returns the
   * current snapshot (post-load). Used by read-only consumers (Review
   * UI sidebar) that don't want to trigger a re-read every mount.
   */
  ensureLoaded: () => Promise<Settings | null>;
  /** Patch the in-memory snapshot. Marks the store dirty. */
  update: (patch: Partial<Settings>) => void;
  /**
   * Persist the current snapshot. Resolves once the write completes.
   * Concurrent callers piggy-back on the in-flight write rather than
   * queueing a second one. Returns `true` on success, `false` on
   * failure (the error message is also written to `error`).
   */
  save: () => Promise<boolean>;
  /**
   * Phase 71 (PR-ι): optimistically flip `onboarded = true` in the
   * in-memory snapshot after the wizard's `heron_mark_onboarded`
   * Tauri call resolves. The Rust side has already written the new
   * value to disk; this keeps `App.tsx`'s first-run detector from
   * needing a second `load()` round-trip to see the new state.
   *
   * Tolerant of `settings === null`: a `markOnboarded()` call before
   * `load()` resolved is a no-op rather than seeding a partial
   * `Settings` object that would mis-render the rest of the form.
   * The wizard always calls `load()` first via `App.tsx`, so the
   * null-guard is defence in depth.
   */
  markOnboarded: () => void;
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
  ensureLoaded: async () => {
    const cached = get().settings;
    if (cached !== null) return cached;
    await get().load();
    return get().settings;
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
    // Wait out any in-flight save, then re-check whether ours still
    // needs to persist. The previous code returned `inFlightSave`
    // directly, which would resolve `true` for a write that never
    // actually persisted the latest state — a real lost-write under
    // rapid mode-pill clicks. The loop catches the 3+-call case where
    // a second save spins up while we were awaiting the first.
    while (inFlightSave !== null) {
      await inFlightSave;
    }
    const { settings, settingsPath } = get();
    if (settings === null || settingsPath === null) {
      const message = "Settings not loaded";
      set({ error: message });
      return false;
    }
    if (!get().dirty) {
      // The piggy-backed in-flight write already persisted whatever we
      // had. Nothing to do.
      return true;
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
  markOnboarded: () => {
    const current = get().settings;
    if (current === null) {
      return;
    }
    if (current.onboarded) {
      return;
    }
    // Mirror the Rust `mark_onboarded` semantics: only the
    // `onboarded` field flips. We deliberately do NOT set `dirty =
    // true` — the disk write happened on the Rust side, so the
    // in-memory snapshot is already consistent with disk.
    set({ settings: { ...current, onboarded: true } });
  },
}));
