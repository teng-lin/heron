/**
 * Settings pane (§16.1).
 *
 * Tabbed form bound to the `Settings` Rust struct via `heron_read_settings`
 * / `heron_write_settings`. Tabs are vertical along the left rail, the
 * shadcn-style form atoms in `components/ui/` carry the look-and-feel,
 * and a Zustand store (`store/settings.ts`) owns the in-memory
 * snapshot + dirty flag.
 *
 * Save is **debounced auto-save (500 ms after the last change)** OR an
 * explicit Save button at the bottom — whichever fires first. The
 * autosave scope is intentional: the user expects most fields to "just
 * stick" without per-field discipline, and Save gives the keyboard
 * crowd an explicit commit. Both paths route through `save()` which
 * coalesces in-flight writes.
 *
 * The page is a thin shell — each tab lives in
 * `pages/settings/tabs/<TabName>.tsx`, helper utilities live under
 * `pages/settings/utils/`, and the larger sub-cards (Recorded apps,
 * Custom shortcuts, Keychain key field) live under
 * `pages/settings/sections/`. Issue #195 split the original 2.4k-line
 * file along those lines so individual tabs can be reviewed in
 * isolation without breaking the autosave / dirty-flag contract.
 *
 * Phase 68 (PR-ζ) extensions:
 *   - Hotkey tab — adds a "Test" button + auto-register-on-save flow
 *     calling `heron_register_hotkey` / `heron_check_hotkey` /
 *     `heron_unregister_hotkey`.
 *   - Audio tab — disk-space gauge via `heron_disk_usage`, retention
 *     radio + "Purge now" via `heron_purge_audio_older_than`.
 *   - About tab — enables "View licenses…" → opens a Radix Dialog
 *     rendering the bundled `THIRD_PARTY_NOTICES.md` via
 *     `react-markdown`.
 *
 * Out of scope for this PR (deliberately):
 * - Keychain plumbing for the API-key field (PR-θ / phase 70).
 * - Real Start/Stop wiring on hotkey trigger — for now the handler
 *   logs + emits `hotkey:fired` for the frontend to react to.
 *
 * The five onboarding probes are wired to `<TestStatus>`; this PR
 * surfaces the Calendar probe only — the rest land with the
 * Onboarding rewrite.
 */

import { useEffect } from "react";
import { Link } from "react-router-dom";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Button } from "../components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../components/ui/tabs";
import { useSettingsStore } from "../store/settings";
import { AUTOSAVE_DEBOUNCE_MS } from "./settings/constants";
import { AboutTab } from "./settings/tabs/AboutTab";
import { AppearanceTab } from "./settings/tabs/AppearanceTab";
import { AudioTab } from "./settings/tabs/AudioTab";
import { CalendarTab } from "./settings/tabs/CalendarTab";
import { GeneralTab } from "./settings/tabs/GeneralTab";
import { HotkeyTab } from "./settings/tabs/HotkeyTab";
import { SummarizerTab } from "./settings/tabs/SummarizerTab";

export default function Settings() {
  const { settings, settingsPath, dirty, loading, saving, error, load, save } =
    useSettingsStore();

  // One-shot load on mount. React 19 + StrictMode mounts effects
  // twice in dev; the store's `load()` coalesces in-flight calls so a
  // second invocation is a cheap no-op.
  useEffect(() => {
    void load();
  }, [load]);

  // Debounced auto-save. Resets the timer on every `dirty` flip OR
  // every snapshot change while dirty — the latter so each in-progress
  // edit re-arms the timer instead of saving 500 ms after the *first*
  // edit. `saving` is also a dep so a second debounced tick fires
  // when an in-flight save completes and the snapshot is still dirty
  // (the store keeps `dirty=true` if `update()` mutated mid-save).
  // Cleanup cancels the timer on unmount or on a new tick; without
  // it, a pending tick would land after the page unmounted and
  // overwrite disk with stale state from a previous render.
  //
  // `persist` is intentionally re-created each render but reads from
  // the Zustand store imperatively, so omitting it from the deps list
  // is safe — the closure can never observe a stale snapshot.
  useEffect(() => {
    if (!dirty || saving) {
      return;
    }
    const handle = setTimeout(() => {
      void persist();
    }, AUTOSAVE_DEBOUNCE_MS);
    return () => clearTimeout(handle);
  }, [dirty, saving, settings]);

  async function persist() {
    const ok = await save();
    if (ok) {
      toast.success("Settings saved");
    } else {
      const msg = useSettingsStore.getState().error ?? "Save failed";
      toast.error(`Save failed: ${msg}`);
    }
  }

  if (loading && settings === null) {
    return (
      <main className="p-6">
        <div className="flex items-center gap-2 text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
          Loading settings…
        </div>
      </main>
    );
  }

  if (error && settings === null) {
    return (
      <main className="p-6 space-y-4">
        <h1 className="text-2xl font-semibold">Settings</h1>
        <p className="text-destructive">Failed to load settings: {error}</p>
        <Button onClick={() => void load()}>Retry</Button>
        <Link to="/home" className="underline">
          Back to home
        </Link>
      </main>
    );
  }

  // `settings === null` only outside the load paths above is
  // unreachable; the type guard satisfies TypeScript and gives the
  // form a stable non-null `settings` to bind against.
  if (settings === null) {
    return null;
  }

  return (
    <main className="p-6" data-testid="settings-page">
      <div className="flex items-center justify-between mb-4">
        <h1 className="text-2xl font-semibold">Settings</h1>
        <Link to="/home" className="text-sm underline text-muted-foreground">
          Back to home
        </Link>
      </div>
      {settingsPath && (
        <p className="mb-4 text-xs text-muted-foreground">
          Stored at <code>{settingsPath}</code>
        </p>
      )}

      <Tabs defaultValue="general" orientation="vertical" className="flex gap-6">
        <TabsList className="w-[180px] shrink-0">
          <TabsTrigger value="general">General</TabsTrigger>
          <TabsTrigger value="appearance">Appearance</TabsTrigger>
          <TabsTrigger value="audio">Audio</TabsTrigger>
          <TabsTrigger value="calendar">Calendar</TabsTrigger>
          <TabsTrigger value="summarizer">Summarizer</TabsTrigger>
          <TabsTrigger value="hotkey">Hotkey</TabsTrigger>
          <TabsTrigger value="about">About</TabsTrigger>
        </TabsList>

        <div className="flex-1 min-w-0">
          <TabsContent value="general">
            <GeneralTab />
          </TabsContent>
          <TabsContent value="appearance">
            <AppearanceTab />
          </TabsContent>
          <TabsContent value="audio">
            <AudioTab />
          </TabsContent>
          <TabsContent value="calendar">
            <CalendarTab />
          </TabsContent>
          <TabsContent value="summarizer">
            <SummarizerTab />
          </TabsContent>
          <TabsContent value="hotkey">
            <HotkeyTab />
          </TabsContent>
          <TabsContent value="about">
            <AboutTab />
          </TabsContent>
        </div>
      </Tabs>

      <div className="mt-6 flex items-center justify-end gap-3 border-t border-border pt-4">
        {dirty && !saving && (
          <span className="text-xs text-muted-foreground">Unsaved changes</span>
        )}
        {saving && (
          <span className="flex items-center gap-1 text-xs text-muted-foreground">
            <Loader2 className="h-3 w-3 animate-spin" aria-hidden="true" />
            Saving…
          </span>
        )}
        <Button onClick={() => void persist()} disabled={saving || !dirty}>
          {saving ? (
            <>
              <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
              Saving
            </>
          ) : (
            "Save"
          )}
        </Button>
      </div>
    </main>
  );
}
