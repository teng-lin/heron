import { useEffect, useState } from "react";

import { Input } from "../../../components/ui/input";
import { useSettingsStore } from "../../../store/settings";
import { DEFAULT_RETENTION_DAYS } from "../constants";

/**
 * Summary-retention picker. Mirrors the Audio retention radio shape
 * (`null` = keep all; `Some(N)` = purge older than N days), but the
 * Tier 4 sweeper (PR #163) only deletes the `.md` summary — never the
 * audio sidecars, which live on `audio_retention_days`.
 */
export function SummaryRetentionField() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);
  const savedDays = settings?.summary_retention_days;
  const [draft, setDraft] = useState<number>(
    savedDays ?? DEFAULT_RETENTION_DAYS,
  );

  // Re-seed `draft` when a load resolves (or another tab edits the
  // saved value). Without this, mounting before `load()` resolves
  // locks the draft at `DEFAULT_RETENTION_DAYS`, and the next radio
  // toggle to "purge" overwrites the user's real saved value.
  useEffect(() => {
    if (typeof savedDays === "number") {
      setDraft(savedDays);
    }
  }, [savedDays]);

  if (settings === null) return null;

  const mode =
    settings.summary_retention_days === null ? "keep_all" : "purge";

  return (
    <fieldset className="space-y-3 pt-4 border-t border-border">
      <legend className="text-sm font-medium">Summary retention</legend>
      <p className="text-xs text-muted-foreground">
        Tier 4 (PR #163). Distinct from the audio sidecar retention
        above — controls how long the markdown summary survives.
      </p>
      <label className="flex items-start gap-2 text-sm cursor-pointer">
        <input
          type="radio"
          name="summary-retention"
          checked={mode === "keep_all"}
          onChange={() => update({ summary_retention_days: null })}
          className="mt-1 h-4 w-4 accent-primary"
        />
        <div>
          <div>Keep all summaries</div>
          <div className="text-xs text-muted-foreground">
            Markdown summaries are never deleted.
          </div>
        </div>
      </label>

      <label className="flex items-start gap-2 text-sm cursor-pointer">
        <input
          type="radio"
          name="summary-retention"
          checked={mode === "purge"}
          onChange={() => update({ summary_retention_days: draft })}
          className="mt-1 h-4 w-4 accent-primary"
        />
        <div className="space-y-2">
          <div>
            Purge summaries older than{" "}
            <Input
              type="number"
              min={1}
              max={3650}
              value={draft}
              onChange={(e) => {
                const raw = e.target.valueAsNumber;
                // Clamp to the same `min` / `max` the input declares —
                // the upper bound was missing, so a paste like
                // "10000" would persist verbatim despite `max={3650}`.
                const next = Number.isNaN(raw)
                  ? DEFAULT_RETENTION_DAYS
                  : Math.min(3650, Math.max(1, Math.floor(raw)));
                setDraft(next);
                if (mode === "purge") {
                  update({ summary_retention_days: next });
                }
              }}
              className="inline-block w-20 mx-1 align-middle"
            />{" "}
            days
          </div>
        </div>
      </label>
    </fieldset>
  );
}
