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

import { useCallback, useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import * as Dialog from "@radix-ui/react-dialog";
import { CheckCircle2, Circle, Loader2 } from "lucide-react";
import ReactMarkdown from "react-markdown";
import { toast } from "sonner";

// Vite's `?raw` import (declared via `vite/client` reference in
// `vite-env.d.ts`) inlines the file as a string at build time. We bundle
// the notices once rather than reading from disk because the Tauri
// app's `frontendDist` may live anywhere on disk relative to the user's
// vault — relying on the renderer's filesystem access here would
// require a fs:read permission we don't otherwise need.
//
// `eslint-disable-next-line` is unnecessary because the project doesn't
// run eslint, but the import path looks unconventional — that's the
// Vite ?raw suffix, which TypeScript's `vite/client` types declare.
import licenseNotices from "../../THIRD_PARTY_NOTICES.md?raw";

import { Button } from "../components/ui/button";
import {
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "../components/ui/dialog";
import { Input } from "../components/ui/input";
import { Label } from "../components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../components/ui/select";
import { Switch } from "../components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../components/ui/tabs";
import { TestStatus } from "../components/TestStatus";
import {
  invoke,
  type DiskUsage,
  type KeychainAccount,
  type TestOutcome,
} from "../lib/invoke";
import { Plus, Trash2 } from "lucide-react";
import { useSettingsStore } from "../store/settings";

/** Debounce window (ms) for auto-save after the last `update()` call. */
const AUTOSAVE_DEBOUNCE_MS = 500;

/**
 * Wire-format strings for `Settings.llm_backend`. Mirrors the
 * `"anthropic" | "openai" | "claude_code_cli" | "codex_cli"` values the
 * Rust side accepts (see `settings.rs`). The desktop side honors the
 * user's choice via `heron_llm::parse_settings_backend` →
 * `select_summarizer_with_user_choice`; an unrecognized string routes
 * to `Preference::Auto`.
 *
 * Grouped visually by hosted-API vs local-CLI per the IA note in the
 * UX-redesign brief: the user reads "API providers" as one billing
 * model and "local CLI" as another.
 */
const LLM_BACKENDS = [
  { value: "anthropic", label: "Anthropic API", group: "api" },
  { value: "openai", label: "OpenAI API", group: "api" },
  { value: "claude_code_cli", label: "Claude Code CLI", group: "cli" },
  { value: "codex_cli", label: "Codex CLI", group: "cli" },
] as const;

/**
 * Anthropic-API model dropdown options. Values are the wire-format
 * model IDs Anthropic's `messages` endpoint expects. The orchestrator
 * does not yet read this back from settings.json — phase 41 (#42)
 * wires backend selection via env vars; the Settings field exists so
 * the data is captured ahead of the orchestrator change.
 */
const ANTHROPIC_MODELS = [
  { value: "claude-opus-4-5", label: "Claude Opus 4.5" },
  { value: "claude-sonnet-4-5", label: "Claude Sonnet 4.5" },
  { value: "claude-haiku-4-5", label: "Claude Haiku 4.5" },
] as const;

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
    <main className="p-6">
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

// ---- Tabs ----------------------------------------------------------

function GeneralTab() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);

  // settings is non-null in render thanks to the parent's guard.
  if (settings === null) return null;

  async function pickVault() {
    try {
      const picked = await openDialog({
        directory: true,
        multiple: false,
        title: "Pick Obsidian vault",
      });
      // The plugin returns `string | string[] | null`; `multiple:
      // false` narrows it to `string | null` at runtime, but the type
      // system can't see that — the explicit guard satisfies both.
      if (typeof picked === "string") {
        update({ vault_root: picked });
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Could not open folder picker: ${message}`);
    }
  }

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">General</h2>

      <div className="space-y-2">
        <Label htmlFor="vault-root">Vault path</Label>
        <div className="flex gap-2">
          <Input
            id="vault-root"
            readOnly
            value={settings.vault_root}
            placeholder="(not set)"
            className="font-mono"
          />
          <Button variant="outline" onClick={() => void pickVault()}>
            Pick…
          </Button>
        </div>
        <p className="text-xs text-muted-foreground">
          Where the markdown summaries land. Empty means the next
          recording will prompt for a folder.
        </p>
      </div>

      <div className="flex items-start justify-between gap-4">
        <div>
          <Label htmlFor="recover-on-launch">Recover on launch</Label>
          <p className="text-xs text-muted-foreground">
            Scan for incomplete sessions when the app starts and offer
            to finish summarizing them.
          </p>
        </div>
        <Switch
          id="recover-on-launch"
          checked={settings.recover_on_launch}
          onCheckedChange={(checked) => update({ recover_on_launch: checked })}
        />
      </div>

      <div className="flex items-start justify-between gap-4">
        <div>
          <Label htmlFor="session-logging">Per-session logs</Label>
          <p className="text-xs text-muted-foreground">
            Write a `heron_session.json` next to each recording for the
            Diagnostics tab.
          </p>
        </div>
        <Switch
          id="session-logging"
          checked={settings.session_logging}
          onCheckedChange={(checked) => update({ session_logging: checked })}
        />
      </div>

      <div className="flex items-start justify-between gap-4">
        <div>
          <Label htmlFor="crash-telemetry">Crash diagnostics</Label>
          <p className="text-xs text-muted-foreground">
            Surface a local diagnostics bundle on crash. heron does not
            send anything off-device.
          </p>
        </div>
        <Switch
          id="crash-telemetry"
          checked={settings.crash_telemetry}
          onCheckedChange={(checked) => update({ crash_telemetry: checked })}
        />
      </div>
    </section>
  );
}

// ---- AppearanceTab -------------------------------------------------

/**
 * Theme preference stored in localStorage["heron:theme"].
 * "system" (or missing key) → follow prefers-color-scheme.
 * "light"  → force light (no data-theme attribute).
 * "dark"   → force dark (data-theme="dark").
 */
type ThemePref = "system" | "light" | "dark";

/**
 * Accent color stored in localStorage["heron:accent"].
 * "" (empty / missing key) → bronze (default, baked into @theme).
 */
type AccentPref = "" | "ink" | "heron" | "sage";

const THEME_OPTIONS: { value: ThemePref; label: string; description: string }[] =
  [
    {
      value: "system",
      label: "System",
      description: "Follow your macOS appearance setting.",
    },
    {
      value: "light",
      label: "Light",
      description: "Always use the light palette.",
    },
    {
      value: "dark",
      label: "Dark",
      description: "Always use the dark palette.",
    },
  ];

const ACCENT_OPTIONS: { value: AccentPref; label: string }[] = [
  { value: "", label: "Bronze (default)" },
  { value: "ink", label: "Ink" },
  { value: "heron", label: "Heron" },
  { value: "sage", label: "Sage" },
];

/**
 * Read the stored theme preference.  Missing key → "system".
 */
function readThemePref(): ThemePref {
  try {
    const stored = localStorage.getItem("heron:theme");
    if (stored === "light" || stored === "dark" || stored === "system") {
      return stored;
    }
  } catch {
    // localStorage not available (unlikely in Tauri, but be defensive)
  }
  return "system";
}

/**
 * Read the stored accent preference.  Missing key → "" (Bronze).
 */
function readAccentPref(): AccentPref {
  try {
    const stored = localStorage.getItem("heron:accent");
    if (stored === "ink" || stored === "heron" || stored === "sage") {
      return stored;
    }
  } catch {
    // localStorage not available
  }
  return "";
}

/**
 * Apply a theme pref to the live document and persist it.
 *
 * "system" → resolve via matchMedia immediately so the page reflects
 * the OS preference without a reload; also remove the stored key so
 * fouc-init.js treats the next launch as "system" too.
 *
 * "light" / "dark" → set / clear data-theme and persist.
 */
function applyTheme(pref: ThemePref): void {
  const html = document.documentElement;

  // Resolve the effective theme to apply to the DOM, independent of storage.
  let effectiveDark: boolean;
  if (pref === "system") {
    effectiveDark =
      window.matchMedia != null &&
      window.matchMedia("(prefers-color-scheme: dark)").matches;
  } else {
    effectiveDark = pref === "dark";
  }

  if (effectiveDark) {
    html.dataset.theme = "dark";
  } else {
    delete html.dataset.theme;
  }

  // Persist the raw preference so fouc-init.js can re-apply it on next launch.
  try {
    if (pref === "system") {
      localStorage.removeItem("heron:theme");
    } else {
      localStorage.setItem("heron:theme", pref);
    }
  } catch {
    // localStorage unavailable — DOM already updated above, so the session
    // reflects the choice even though it won't survive a reload.
  }
}

/**
 * Apply an accent pref to the live document and persist it.
 *
 * "" (Bronze) → remove data-accent and remove the localStorage key
 * so fouc-init.js restores the default on next launch.
 */
function applyAccent(pref: AccentPref): void {
  const html = document.documentElement;

  // Update the DOM first — independent of storage availability.
  if (pref === "") {
    delete html.dataset.accent;
  } else {
    html.dataset.accent = pref;
  }

  // Persist so fouc-init.js can re-apply on next launch.
  try {
    if (pref === "") {
      localStorage.removeItem("heron:accent");
    } else {
      localStorage.setItem("heron:accent", pref);
    }
  } catch {
    // localStorage unavailable — DOM already updated above.
  }
}

function AppearanceTab() {
  const [theme, setTheme] = useState<ThemePref>(readThemePref);
  const [accent, setAccent] = useState<AccentPref>(readAccentPref);

  // Keep the DOM in sync with OS theme changes while "system" is selected.
  useEffect(() => {
    if (theme !== "system") return;
    const mq = window.matchMedia?.("(prefers-color-scheme: dark)");
    if (!mq) return;
    const handler = () => applyTheme("system");
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, [theme]);

  function handleThemeChange(value: ThemePref) {
    setTheme(value);
    applyTheme(value);
  }

  function handleAccentChange(value: AccentPref) {
    setAccent(value);
    applyAccent(value);
  }

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Appearance</h2>

      <fieldset className="space-y-3">
        <legend className="text-sm font-medium">Theme</legend>
        <div className="space-y-2">
          {THEME_OPTIONS.map((opt) => (
            <label
              key={opt.value}
              className="flex items-start gap-2 text-sm cursor-pointer"
            >
              <input
                type="radio"
                name="theme"
                value={opt.value}
                checked={theme === opt.value}
                onChange={() => handleThemeChange(opt.value)}
                className="mt-0.5 h-4 w-4 accent-primary"
              />
              <div>
                <div>{opt.label}</div>
                <div className="text-xs text-muted-foreground">
                  {opt.description}
                </div>
              </div>
            </label>
          ))}
        </div>
      </fieldset>

      <fieldset className="space-y-3">
        <legend className="text-sm font-medium">Accent color</legend>
        <div className="space-y-2">
          {ACCENT_OPTIONS.map((opt) => (
            <label
              key={opt.value === "" ? "bronze" : opt.value}
              className="flex items-center gap-2 text-sm cursor-pointer"
            >
              <input
                type="radio"
                name="accent"
                value={opt.value}
                checked={accent === opt.value}
                onChange={() => handleAccentChange(opt.value)}
                className="h-4 w-4 accent-primary"
              />
              {opt.label}
            </label>
          ))}
        </div>
        <p className="text-xs text-muted-foreground">
          Changes apply immediately. Reload is not required.
        </p>
      </fieldset>
    </section>
  );
}

// ---- AudioTab ------------------------------------------------------

/**
 * Default retention window when the user flips the radio to "purge"
 * without typing a number. 30 days is the §16.1 brief's example
 * value — a month of audio retention is the median ask from the
 * design-partner interviews documented in `docs/scope-fixes.md`.
 */
const DEFAULT_RETENTION_DAYS = 30;

/** How often we re-poll the disk-usage gauge while the Audio tab is open. */
const DISK_USAGE_POLL_MS = 5000;

/**
 * Humanize a byte count for the disk-usage gauge. SI units (1000-step),
 * not IEC, so the displayed number matches what macOS Finder reports.
 * Returns e.g. `"1.4 GB"` / `"38 MB"` / `"512 B"`.
 */
function formatBytes(bytes: number): string {
  if (bytes < 1000) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"] as const;
  let value = bytes;
  let unit = -1;
  do {
    value /= 1000;
    unit += 1;
  } while (value >= 1000 && unit < units.length - 1);
  // One decimal for KB/MB/GB; bytes <1000 already returned above.
  // `toFixed(1)` keeps the trailing `.0` so "1.0 GB" doesn't visually
  // jitter into "1 GB" between polls.
  return `${value.toFixed(1)} ${units[unit] ?? "TB"}`;
}

function AudioTab() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);
  const [usage, setUsage] = useState<DiskUsage | null>(null);
  const [usageError, setUsageError] = useState<string | null>(null);
  const [purging, setPurging] = useState(false);
  const [confirmingPurge, setConfirmingPurge] = useState(false);
  // The number input shows `retentionDraft` while the radio is on
  // "Keep all" (so flipping to "purge" picks up the user's last
  // typed value); when the radio is on "purge" the draft is the
  // canonical source — every keystroke re-saves via `update()`.
  // Seeded from the loaded settings so a returning user sees their
  // saved value, not a hardcoded default.
  const [retentionDraft, setRetentionDraft] = useState<number>(
    settings?.audio_retention_days ?? DEFAULT_RETENTION_DAYS,
  );

  // Re-poll disk usage on tab focus + periodic refresh while open. The
  // poll is cheap (single non-recursive `read_dir`) and lets the gauge
  // reflect a `Purge now` outcome without forcing a manual refresh.
  // The vault path is captured at effect-setup time so subsequent
  // edits to other Settings fields (which re-render the component but
  // don't change `vault_root`) don't churn the polling timer.
  const vaultRoot = settings?.vault_root ?? "";
  useEffect(() => {
    if (vaultRoot === "") {
      setUsage(null);
      setUsageError(null);
      return;
    }
    let cancelled = false;
    async function refresh() {
      try {
        const result = await invoke("heron_disk_usage", {
          vaultPath: vaultRoot,
        });
        if (!cancelled) {
          setUsage(result);
          setUsageError(null);
        }
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        if (!cancelled) {
          setUsage(null);
          setUsageError(message);
        }
      }
    }
    void refresh();
    const handle = setInterval(() => void refresh(), DISK_USAGE_POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(handle);
    };
  }, [vaultRoot]);

  if (settings === null) return null;

  const purgeMode = settings.audio_retention_days === null ? "keep_all" : "purge";

  function setKeepAll() {
    update({ audio_retention_days: null });
  }

  function setPurgeMode() {
    update({ audio_retention_days: retentionDraft });
  }

  async function runPurge() {
    setConfirmingPurge(false);
    if (settings === null) return;
    if (settings.vault_root === "") {
      toast.error("Pick a vault path on the General tab first.");
      return;
    }
    const days = settings.audio_retention_days ?? retentionDraft;
    setPurging(true);
    try {
      const count = await invoke("heron_purge_audio_older_than", {
        vaultPath: settings.vault_root,
        days,
      });
      toast.success(
        count === 0
          ? "Nothing to purge — no audio older than the threshold."
          : `Purged ${count} audio file${count === 1 ? "" : "s"}.`,
      );
      // Re-poll so the gauge reflects the deletion immediately rather
      // than waiting for the next interval tick.
      try {
        const next = await invoke("heron_disk_usage", {
          vaultPath: settings.vault_root,
        });
        setUsage(next);
      } catch {
        // Gauge refresh failure is non-fatal — the next poll will retry.
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      toast.error(`Purge failed: ${message}`);
    } finally {
      setPurging(false);
    }
  }

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Audio</h2>

      <RecordedAppsCard />

      <div className="rounded-md border border-border p-4 space-y-1">
        <div className="text-sm font-medium">Disk usage</div>
        {settings.vault_root === "" ? (
          <p className="text-xs text-muted-foreground">
            Pick a vault path on the General tab to see disk usage.
          </p>
        ) : usageError !== null ? (
          <p className="text-xs text-destructive">{usageError}</p>
        ) : usage === null ? (
          <p className="text-xs text-muted-foreground">Loading…</p>
        ) : (
          <p className="text-sm">
            {formatBytes(usage.vault_bytes)} across{" "}
            {usage.vault_session_count} session
            {usage.vault_session_count === 1 ? "" : "s"}
          </p>
        )}
      </div>

      <fieldset className="space-y-3">
        <legend className="text-sm font-medium">Audio retention</legend>
        <label className="flex items-start gap-2 text-sm cursor-pointer">
          <input
            type="radio"
            name="retention"
            checked={purgeMode === "keep_all"}
            onChange={setKeepAll}
            className="mt-1 h-4 w-4 accent-primary"
          />
          <div>
            <div>Keep all audio</div>
            <div className="text-xs text-muted-foreground">
              `.wav` and `.m4a` files stay next to each session's `.md`
              forever. Choose this if you re-process recordings.
            </div>
          </div>
        </label>

        <label className="flex items-start gap-2 text-sm cursor-pointer">
          <input
            type="radio"
            name="retention"
            checked={purgeMode === "purge"}
            onChange={setPurgeMode}
            className="mt-1 h-4 w-4 accent-primary"
          />
          <div className="space-y-2">
            <div>
              Purge audio older than{" "}
              <Input
                type="number"
                min={1}
                max={3650}
                value={retentionDraft}
                onChange={(e) => {
                  const raw = e.target.valueAsNumber;
                  const next = Number.isNaN(raw)
                    ? DEFAULT_RETENTION_DAYS
                    : Math.max(1, Math.floor(raw));
                  setRetentionDraft(next);
                  if (purgeMode === "purge") {
                    update({ audio_retention_days: next });
                  }
                }}
                className="inline-block w-20 mx-1 align-middle"
              />{" "}
              days, keep transcript + .md
            </div>
            <div className="text-xs text-muted-foreground">
              The `.md` summary and the transcript inside it are
              **never** purged — only the audio sidecars.
            </div>
          </div>
        </label>
      </fieldset>

      <div>
        <Button
          variant="outline"
          onClick={() => setConfirmingPurge(true)}
          disabled={
            purging ||
            settings.vault_root === "" ||
            settings.audio_retention_days === null
          }
        >
          {purging ? (
            <>
              <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
              Purging…
            </>
          ) : (
            "Purge now"
          )}
        </Button>
        {settings.audio_retention_days === null && (
          <p className="mt-1 text-xs text-muted-foreground">
            Switch retention to "purge older than" first to enable
            on-demand purge.
          </p>
        )}
      </div>

      <Dialog.Root open={confirmingPurge} onOpenChange={setConfirmingPurge}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Purge audio sidecars?</DialogTitle>
          </DialogHeader>
          <p className="text-sm">
            Delete `.wav` and `.m4a` files older than{" "}
            <strong>
              {settings.audio_retention_days ?? retentionDraft} days
            </strong>{" "}
            in <code className="font-mono">{settings.vault_root}</code>?
            The transcript and `.md` summary stay.
          </p>
          <div className="flex justify-end gap-2 mt-2">
            <Button
              variant="outline"
              onClick={() => setConfirmingPurge(false)}
              disabled={purging}
            >
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={() => void runPurge()}
              disabled={purging}
            >
              Purge
            </Button>
          </div>
        </DialogContent>
      </Dialog.Root>

      <div className="space-y-2 pt-4 border-t border-border">
        <Label htmlFor="remind-interval">Disclosure-banner reminder (seconds)</Label>
        <Input
          id="remind-interval"
          type="number"
          min={0}
          max={3600}
          value={settings.remind_interval_secs}
          onChange={(e) => {
            // Empty input is a numeric "0"; clamp to non-negative
            // since the Rust side stores `u32`.
            const raw = e.target.valueAsNumber;
            const next = Number.isNaN(raw) ? 0 : Math.max(0, Math.floor(raw));
            update({ remind_interval_secs: next });
          }}
        />
        <p className="text-xs text-muted-foreground">
          §14.2: the recording badge re-flashes on this cadence so the
          recorded party is reminded the call is being captured.
        </p>
      </div>

      <div className="space-y-2">
        <Label htmlFor="min-free-disk">Stop-recording threshold (MiB free)</Label>
        <Input
          id="min-free-disk"
          type="number"
          min={0}
          max={1048576}
          value={settings.min_free_disk_mib}
          onChange={(e) => {
            const raw = e.target.valueAsNumber;
            const next = Number.isNaN(raw) ? 0 : Math.max(0, Math.floor(raw));
            update({ min_free_disk_mib: next });
          }}
        />
        <p className="text-xs text-muted-foreground">
          Recording is disabled when free disk drops below this amount.
        </p>
      </div>
    </section>
  );
}

/**
 * PR-λ (phase 73) — Settings → Audio "Recorded apps" card.
 *
 * Renders one row per bundle ID in `settings.target_bundle_ids` with
 * an `<input>` and a remove button, plus a quick-add preset dropdown
 * and an "+ Add app" button that appends an empty row. Validates
 * non-empty + unique entries on save (the existing 500ms debounce
 * autosave path handles persistence).
 *
 * Why a sub-component: the Audio tab is already long, and the picker
 * keeps a couple of pieces of UI-only state (the validation banner,
 * the preset dropdown selection) that don't belong on the parent.
 */
const PRESET_BUNDLES = [
  { value: "us.zoom.xos", label: "Zoom" },
  { value: "com.microsoft.teams2", label: "Microsoft Teams" },
  { value: "com.google.Chrome", label: "Google Chrome" },
] as const;

/**
 * Validate the user's `target_bundle_ids` list. Returns flags the
 * Settings card uses to render an inline error banner.
 *
 * - `hasEmpty` — at least one row is blank / whitespace-only.
 * - `hasDupe` — two or more rows have the same trimmed bundle ID.
 *   Empty rows are excluded from the duplicate check (they're flagged
 *   separately by `hasEmpty`).
 */
function validateBundleIds(targets: string[]): {
  hasEmpty: boolean;
  hasDupe: boolean;
} {
  const trimmed = targets.map((t) => t.trim());
  const nonEmpty = trimmed.filter((t) => t !== "");
  return {
    hasEmpty: trimmed.length !== nonEmpty.length,
    hasDupe: nonEmpty.length !== new Set(nonEmpty).size,
  };
}

function RecordedAppsCard() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);
  // Local state for the preset dropdown — `<select>` value is the
  // bundle ID about to be added; resets to empty after each Add.
  const [presetPick, setPresetPick] = useState<string>("");
  // Editor copy of the bundle-ID list. The brief calls for "validates
  // non-empty + unique on save" — we honor that by keeping in-progress
  // edits *local* until the list is valid, then promoting them to
  // the Zustand store (which dirties the autosave). Without this, a
  // half-edited row would pass through to disk via the 500 ms debounce
  // and the user would persist an empty / duplicate target.
  const storeTargets = settings?.target_bundle_ids;
  const [editorTargets, setEditorTargets] = useState<string[]>(
    storeTargets ?? [],
  );
  const lastSyncedRef = useRef<string[] | null>(null);

  // Mirror an external store change (load, or another tab editing the
  // same field) into the editor copy. Reference-equal store updates
  // skip the reset so a save that lands while the user is mid-edit
  // doesn't clobber their typing — only a *different* incoming value
  // (post-load, post-external-change) re-seeds the editor.
  useEffect(() => {
    if (storeTargets === undefined) return;
    if (storeTargets === lastSyncedRef.current) return;
    setEditorTargets(storeTargets);
    lastSyncedRef.current = storeTargets;
  }, [storeTargets]);

  if (settings === null) return null;

  /** Promote `next` to the editor + (when valid) to the store. */
  function commit(next: string[]) {
    setEditorTargets(next);
    const { hasEmpty, hasDupe } = validateBundleIds(next);
    if (!hasEmpty && !hasDupe) {
      lastSyncedRef.current = next;
      update({ target_bundle_ids: next });
    }
  }

  function setRow(idx: number, value: string) {
    const next = editorTargets.slice();
    next[idx] = value;
    commit(next);
  }

  function removeRow(idx: number) {
    const next = editorTargets.slice();
    next.splice(idx, 1);
    // Defence in depth: an empty list would silently disable
    // recording. Clamp to "at least Zoom" so the user can't strand
    // themselves with a no-op tap target. The user can edit the
    // remaining row to a different bundle ID; they can't delete it.
    if (next.length === 0) {
      next.push("us.zoom.xos");
    }
    commit(next);
  }

  function addRow(initial: string) {
    commit([...editorTargets, initial]);
  }

  const targets = editorTargets;
  const { hasEmpty, hasDupe } = validateBundleIds(targets);

  return (
    <div className="rounded-md border border-border p-4 space-y-3">
      <div className="text-sm font-medium">Recorded apps</div>
      <p className="text-xs text-muted-foreground">
        heron taps audio from these apps. Add a Microsoft Teams or
        Google Chrome bundle ID to record those too.
      </p>
      <div className="space-y-2">
        {targets.map((id, idx) => (
          <div
            // Index-keyed: bundle IDs aren't unique while the user is
            // editing (a fresh row is "" until they type). React's key
            // must be stable across the per-keystroke re-renders, and
            // the index is the only stable handle while values mutate.
            // eslint-disable-next-line react/no-array-index-key
            key={idx}
            className="flex items-center gap-2"
          >
            <Input
              value={id}
              onChange={(e) => setRow(idx, e.target.value)}
              placeholder="e.g. us.zoom.xos"
              className="font-mono"
              aria-label={`Bundle ID ${idx + 1}`}
            />
            <Button
              variant="ghost"
              size="icon"
              onClick={() => removeRow(idx)}
              aria-label={`Remove ${id || "row"}`}
              // Dim-but-clickable when only one row remains; the
              // `removeRow` path clamps to ["us.zoom.xos"] so the user
              // recovers from an "all gone" state without a reset
              // button. Disabling the button entirely would force the
              // user to pick a different deletion strategy.
              title="Remove"
            >
              <Trash2 className="h-4 w-4" aria-hidden="true" />
            </Button>
          </div>
        ))}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <Button
          variant="outline"
          size="sm"
          onClick={() => addRow("")}
          aria-label="Add app"
        >
          <Plus className="h-3.5 w-3.5" aria-hidden="true" />
          Add app
        </Button>
        <span className="text-xs text-muted-foreground">Quick add:</span>
        <select
          aria-label="Quick-add preset"
          className={
            "h-8 rounded-md border border-border bg-background px-2 " +
            "text-xs focus-visible:outline-none focus-visible:ring-2 " +
            "focus-visible:ring-primary"
          }
          value={presetPick}
          onChange={(e) => {
            const v = e.target.value;
            if (v === "") return;
            // Skip duplicates silently — the user clicked the preset
            // expecting "make sure this is in the list", so a no-op
            // result on a duplicate is the right semantic.
            if (!targets.includes(v)) {
              addRow(v);
            }
            setPresetPick("");
          }}
        >
          <option value="">Choose…</option>
          {PRESET_BUNDLES.map((p) => (
            <option key={p.value} value={p.value}>
              {p.label} ({p.value})
            </option>
          ))}
        </select>
      </div>
      {(hasEmpty || hasDupe) && (
        <p className="text-xs text-destructive">
          {hasEmpty && "Bundle IDs must not be empty."}
          {hasEmpty && hasDupe && " "}
          {hasDupe && "Each bundle ID must be unique."}
        </p>
      )}
    </div>
  );
}

function CalendarTab() {
  const [outcome, setOutcome] = useState<TestOutcome | null>(null);
  const [testing, setTesting] = useState(false);

  async function runTest() {
    setTesting(true);
    try {
      const result = await invoke("heron_test_calendar");
      setOutcome(result);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setOutcome({ status: "fail", details: message });
    } finally {
      setTesting(false);
    }
  }

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Calendar</h2>

      <p className="text-sm text-muted-foreground">
        heron reads a one-hour Calendar window when a recording starts
        to attribute the meeting title and attendees. Calendar access
        is read-only and never leaves the device.
      </p>

      <div className="space-y-3">
        <Button
          variant="outline"
          onClick={() => void runTest()}
          disabled={testing}
        >
          {testing ? (
            <>
              <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
              Testing…
            </>
          ) : (
            "Test calendar access"
          )}
        </Button>
        <TestStatus outcome={outcome} />
      </div>
    </section>
  );
}

function SummarizerTab() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);
  // The model picker is ephemeral until the Rust `Settings` struct grows
  // an `llm_model` field — local state lets the user move the dropdown
  // visually without losing the change to a hardcoded controlled value.
  // The default is the middle option (Sonnet) which matches the
  // orchestrator's current cost-aware default (phase 41 / #42).
  const [anthropicModel, setAnthropicModel] = useState<string>(
    ANTHROPIC_MODELS[1].value,
  );

  if (settings === null) return null;

  const showAnthropicModelPicker = settings.llm_backend === "anthropic";

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Summarizer</h2>

      <fieldset className="space-y-2">
        <legend className="text-sm font-medium">LLM backend</legend>
        <div className="space-y-2">
          {LLM_BACKENDS.map((opt) => (
            <label
              key={opt.value}
              className="flex items-center gap-2 text-sm cursor-pointer"
            >
              <input
                type="radio"
                name="llm-backend"
                value={opt.value}
                checked={settings.llm_backend === opt.value}
                onChange={() => update({ llm_backend: opt.value })}
                className="h-4 w-4 accent-primary"
              />
              {opt.label}
            </label>
          ))}
        </div>
      </fieldset>

      {showAnthropicModelPicker && (
        <div className="space-y-2">
          <Label htmlFor="anthropic-model">Anthropic model</Label>
          <Select value={anthropicModel} onValueChange={setAnthropicModel}>
            <SelectTrigger id="anthropic-model" className="w-72">
              <SelectValue placeholder="Pick a model" />
            </SelectTrigger>
            <SelectContent>
              {ANTHROPIC_MODELS.map((m) => (
                <SelectItem key={m.value} value={m.value}>
                  {m.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <p className="text-xs text-muted-foreground">
            Model selection persists once the orchestrator reads it from
            settings.json (follow-up — the Rust `Settings` struct has no
            `llm_model` field today).
          </p>
        </div>
      )}

      <KeychainKeyField
        account="anthropic_api_key"
        label="Anthropic API key"
        placeholder="sk-ant-…"
        helpText="Stored in the macOS login Keychain. heron never writes API keys to settings.json or any other file on disk."
      />

      <KeychainKeyField
        account="openai_api_key"
        label="OpenAI API key"
        placeholder="sk-…"
        helpText="Used by the OpenAI Realtime backend during meetings and by the Codex CLI summarizer. Stored in the macOS login Keychain — never written to settings.json or any other file on disk."
      />

      <div className="flex items-start justify-between gap-4">
        <div>
          <Label htmlFor="auto-summarize">Auto-summarize on stop</Label>
          <p className="text-xs text-muted-foreground">
            When a recording ends, kick off the summarizer immediately.
            Off means the review UI gets a "summarize" button.
          </p>
        </div>
        <Switch
          id="auto-summarize"
          checked={settings.auto_summarize}
          onCheckedChange={(checked) => update({ auto_summarize: checked })}
        />
      </div>
    </section>
  );
}

/**
 * Result of `heron_check_hotkey` rendered next to the Test button.
 * Tri-state because a fresh tab visit shouldn't show stale "free" /
 * "conflict" copy from the previous chord.
 */
type ConflictState = "unknown" | "free" | "conflict" | "checking";

function HotkeyTab() {
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
    </section>
  );
}

function AboutTab() {
  return (
    <section className="space-y-4">
      <h2 className="text-lg font-medium">About</h2>
      <dl className="grid grid-cols-[8rem_1fr] gap-y-2 text-sm">
        <dt className="text-muted-foreground">Version</dt>
        <dd>{__APP_VERSION__}</dd>
        <dt className="text-muted-foreground">Build</dt>
        <dd>{__APP_BUILD__}</dd>
      </dl>
      <Dialog.Root>
        <DialogTrigger asChild>
          <Button variant="outline">View licenses…</Button>
        </DialogTrigger>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Third-party notices</DialogTitle>
          </DialogHeader>
          {/*
           * `prose` styling lives inline since the design system doesn't
           * (yet) ship a Tailwind typography preset. The component
           * overrides give us readable headings + spacing without
           * a `prose` plugin dep. Limiting to common nodes — paragraph,
           * heading, link, list, code, strong — matches the markdown
           * subset the bundled notices file uses.
           */}
          <div className="text-sm leading-relaxed space-y-3">
            <ReactMarkdown
              components={{
                h1: ({ children }) => (
                  <h1 className="text-lg font-semibold mt-4 first:mt-0">
                    {children}
                  </h1>
                ),
                h2: ({ children }) => (
                  <h2 className="text-base font-semibold mt-4">{children}</h2>
                ),
                h3: ({ children }) => (
                  <h3 className="text-sm font-semibold mt-3">{children}</h3>
                ),
                p: ({ children }) => <p className="my-2">{children}</p>,
                a: ({ href, children }) => (
                  <a
                    href={href}
                    className="underline text-primary hover:opacity-80"
                    target="_blank"
                    rel="noopener noreferrer"
                  >
                    {children}
                  </a>
                ),
                ul: ({ children }) => (
                  <ul className="list-disc pl-6 my-2 space-y-1">{children}</ul>
                ),
                ol: ({ children }) => (
                  <ol className="list-decimal pl-6 my-2 space-y-1">
                    {children}
                  </ol>
                ),
                code: ({ children }) => (
                  <code className="font-mono text-xs bg-muted px-1 py-0.5 rounded">
                    {children}
                  </code>
                ),
                strong: ({ children }) => (
                  <strong className="font-semibold">{children}</strong>
                ),
                hr: () => <hr className="my-4 border-border" />,
              }}
            >
              {licenseNotices}
            </ReactMarkdown>
          </div>
        </DialogContent>
      </Dialog.Root>
      <p className="text-xs text-muted-foreground">
        heron is private, on-device, and AGPL-3.0-or-later licensed.
      </p>
    </section>
  );
}

// ---- Keychain field (PR-θ / phase 70) ------------------------------

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
function KeychainKeyField({
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

/**
 * Map a `KeyboardEvent.key` value to Tauri's `tauri-plugin-global-shortcut`
 * key spelling. The browser's `key` is mostly correct already (`F1`,
 * `R`, `Enter`); a small alias table covers the spaces ("ArrowLeft" →
 * "Left") that diverge.
 */
function normalizeKey(key: string): string {
  // Space is `length === 1` but Tauri's parser wants the literal
  // word "Space", so it has to be handled before the single-char
  // uppercase fast path.
  if (key === " ") {
    return "Space";
  }
  if (key.length === 1) {
    return key.toUpperCase();
  }
  switch (key) {
    case "ArrowLeft":
      return "Left";
    case "ArrowRight":
      return "Right";
    case "ArrowUp":
      return "Up";
    case "ArrowDown":
      return "Down";
    default:
      return key;
  }
}
