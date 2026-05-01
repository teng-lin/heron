import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { toast } from "sonner";

import { Button } from "../../../components/ui/button";
import { Input } from "../../../components/ui/input";
import { Label } from "../../../components/ui/label";
import { Switch } from "../../../components/ui/switch";
import { useSettingsStore } from "../../../store/settings";
import { FILE_NAMING_PATTERNS } from "../constants";

export function GeneralTab() {
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
    <section className="space-y-6" data-testid="settings-tab-general">
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

      <div className="rounded-md border border-border p-4 space-y-3">
        <div>
          <Label className="text-sm font-medium">Your context</Label>
          <p className="text-xs text-muted-foreground">
            Tier 4 (PR #167) feeds these into the summarizer's system
            prompt — the LLM sees who is taking the meeting and what
            they're working on. Leave blank to omit.
          </p>
        </div>
        <div className="space-y-2">
          <Label htmlFor="persona-name">Your name</Label>
          <Input
            id="persona-name"
            value={settings.persona.name}
            onChange={(e) =>
              update({
                persona: { ...settings.persona, name: e.target.value },
              })
            }
            placeholder="e.g. Maya Patel"
          />
        </div>
        <div className="space-y-2">
          <Label htmlFor="persona-role">Your role</Label>
          <Input
            id="persona-role"
            value={settings.persona.role}
            onChange={(e) =>
              update({
                persona: { ...settings.persona, role: e.target.value },
              })
            }
            placeholder="e.g. Founding engineer at Acme"
          />
        </div>
        <div className="space-y-2">
          <Label htmlFor="persona-working-on">What you're working on</Label>
          <Input
            id="persona-working-on"
            value={settings.persona.working_on}
            onChange={(e) =>
              update({
                persona: { ...settings.persona, working_on: e.target.value },
              })
            }
            placeholder="e.g. Migration from Postgres 14 to 16"
          />
        </div>
      </div>

      <fieldset className="space-y-3">
        <legend className="text-sm font-medium">Vault file naming</legend>
        <p className="text-xs text-muted-foreground">
          Tier 4 (PR #168). Controls how each session's `.md` filename is
          derived. Existing files are not renamed; only new sessions
          adopt the pattern.
        </p>
        <div className="space-y-2">
          {FILE_NAMING_PATTERNS.map((opt) => (
            <label
              key={opt.value}
              className="flex items-start gap-2 text-sm cursor-pointer"
            >
              <input
                type="radio"
                name="file-naming-pattern"
                value={opt.value}
                checked={settings.file_naming_pattern === opt.value}
                onChange={() => update({ file_naming_pattern: opt.value })}
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
    </section>
  );
}
