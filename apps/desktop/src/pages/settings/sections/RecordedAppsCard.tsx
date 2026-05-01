import { useEffect, useRef, useState } from "react";
import { Plus, Trash2 } from "lucide-react";

import { Button } from "../../../components/ui/button";
import { Input } from "../../../components/ui/input";
import { useSettingsStore } from "../../../store/settings";
import { PRESET_BUNDLES } from "../constants";
import { validateBundleIds } from "../utils/bundle-ids";

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
export function RecordedAppsCard() {
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
      // `validateBundleIds` checks against the *trimmed* form; persist
      // the trimmed form too. A pasted bundle ID with leading/trailing
      // whitespace would otherwise pass validation and then fail to
      // match the LSApplicationIdentifier on the macOS side.
      const trimmed = next.map((id) => id.trim());
      lastSyncedRef.current = trimmed;
      update({ target_bundle_ids: trimmed });
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
