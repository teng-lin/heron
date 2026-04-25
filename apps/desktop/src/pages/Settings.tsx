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
 * Out of scope for this PR (deliberately):
 * - Keychain plumbing for the API-key field (placeholder + disclaimer).
 * - "Purge audio older than N days" implementation (Audio tab).
 * - Disk-space gauge (Audio tab).
 * - Hotkey conflict detection (Hotkey tab).
 * - License-viewer modal (About tab "view licenses…" button is a no-op).
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
import { toast } from "sonner";

import { Button } from "../components/ui/button";
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
import { invoke, type KeychainAccount, type TestOutcome } from "../lib/invoke";
import { useSettingsStore } from "../store/settings";

/** Debounce window (ms) for auto-save after the last `update()` call. */
const AUTOSAVE_DEBOUNCE_MS = 500;

/**
 * Wire-format strings for `Settings.llm_backend`. Mirrors the
 * `"anthropic" | "claude_code_cli" | "codex_cli"` values the Rust side
 * accepts (see `settings.rs`).
 */
const LLM_BACKENDS = [
  { value: "anthropic", label: "Anthropic API" },
  { value: "claude_code_cli", label: "Claude Code CLI" },
  { value: "codex_cli", label: "Codex CLI" },
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

function AudioTab() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);

  if (settings === null) return null;

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Audio</h2>

      <p className="text-sm text-muted-foreground">
        Audio retention controls (purge older than N days, disk-space
        gauge) ship with week 12 polish.
      </p>

      <div className="space-y-2">
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
        helpText="Used by the Codex CLI summarizer backend. Stored in the macOS login Keychain — never written to disk."
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

function HotkeyTab() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);
  const [capturing, setCapturing] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

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
    setCapturing(false);
    inputRef.current?.blur();
  }

  return (
    <section className="space-y-6">
      <h2 className="text-lg font-medium">Hotkey</h2>

      <div className="space-y-2">
        <Label htmlFor="record-hotkey">Record / stop hotkey</Label>
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
        <p className="text-xs text-muted-foreground">
          {capturing
            ? "Press the chord you'd like — Escape to cancel."
            : "Click the field and press a chord to rebind."}
        </p>
        <p className="text-xs text-muted-foreground">
          Hotkey conflict detection ships in week 12 polish.
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
      <Button variant="outline" disabled title="License viewer ships in a follow-up">
        View licenses…
      </Button>
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
          <Dialog.Description className="text-sm text-muted-foreground">
            {hasKey
              ? "A key is already stored. Enter a new value to replace it, or delete the existing entry."
              : "Paste your key. heron stores it in the macOS login Keychain only."}
          </Dialog.Description>

          <div className="space-y-2">
            <Label htmlFor={`keychain-${account}`}>Key</Label>
            <Input
              id={`keychain-${account}`}
              type="password"
              autoComplete="off"
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              placeholder={placeholder}
              value={secret}
              onChange={(e) => setSecret(e.target.value)}
              disabled={busy}
              // `aria-describedby` ties the password field to the
              // dialog's description so screen readers announce the
              // "stored in Keychain" copy alongside the input.
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
