import { useEffect, useState } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

import {
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../../../components/ui/dialog";
import { Button } from "../../../components/ui/button";
import { Input } from "../../../components/ui/input";
import { Label } from "../../../components/ui/label";
import { Switch } from "../../../components/ui/switch";
import { invoke, type DiskUsage } from "../../../lib/invoke";
import { useSettingsStore } from "../../../store/settings";
import { DEFAULT_RETENTION_DAYS, DISK_USAGE_POLL_MS } from "../constants";
import { HotwordsField } from "../sections/HotwordsField";
import { RecordedAppsCard } from "../sections/RecordedAppsCard";
import { SummaryRetentionField } from "../sections/SummaryRetentionField";
import { formatBytes } from "../utils/format-bytes";

export function AudioTab() {
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
  const savedAudioDays = settings?.audio_retention_days;
  const [retentionDraft, setRetentionDraft] = useState<number>(
    savedAudioDays ?? DEFAULT_RETENTION_DAYS,
  );

  // Re-seed `retentionDraft` once the saved value lands. Without
  // this, mounting before `load()` resolves locks the draft at
  // `DEFAULT_RETENTION_DAYS`, and the next "purge" radio toggle
  // overwrites the user's real saved value. Same shape as the
  // `SummaryRetentionField` re-seed.
  useEffect(() => {
    if (typeof savedAudioDays === "number") {
      setRetentionDraft(savedAudioDays);
    }
  }, [savedAudioDays]);

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

      <SummaryRetentionField />

      <HotwordsField />

      <div className="flex items-start justify-between gap-4 pt-4 border-t border-border">
        <div>
          <Label htmlFor="show-tray-indicator">Show tray indicator</Label>
          <p className="text-xs text-muted-foreground">
            Tier 4 (PR #170). When off, the menu-bar icon stops cycling
            through Recording / Transcribing / Summarizing colors but
            stays clickable.
          </p>
        </div>
        <Switch
          id="show-tray-indicator"
          checked={settings.show_tray_indicator}
          onCheckedChange={(checked) => update({ show_tray_indicator: checked })}
        />
      </div>

      <div className="flex items-start justify-between gap-4">
        <div>
          <Label htmlFor="auto-detect-meeting-app">Auto-detect meeting apps</Label>
          <p className="text-xs text-muted-foreground">
            Tier 4 (PR #170). Lets heron prime a recording when a
            configured meeting app launches. Manual record (button /
            hotkey) is unaffected.
          </p>
        </div>
        <Switch
          id="auto-detect-meeting-app"
          checked={settings.auto_detect_meeting_app}
          onCheckedChange={(checked) =>
            update({ auto_detect_meeting_app: checked })
          }
        />
      </div>
    </section>
  );
}
