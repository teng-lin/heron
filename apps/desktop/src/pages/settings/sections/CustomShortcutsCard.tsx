import { useEffect, useRef, useState } from "react";
import { Plus, Trash2 } from "lucide-react";

import { Button } from "../../../components/ui/button";
import { Input } from "../../../components/ui/input";
import { useSettingsStore } from "../../../store/settings";
import { KNOWN_SHORTCUT_ACTIONS } from "../constants";

/**
 * Tier 1 / Tier 4 (PR #164) shortcuts table. Persists a
 * `BTreeMap<ActionId, Accelerator>` that the Rust startup hook
 * iterates and registers via `tauri-plugin-global-shortcut`.
 *
 * Rendering shape: one row per (action_id, accelerator) entry with
 * inline edits + remove. An "Add shortcut" button appends a blank row
 * the user can fill in. Empty / duplicate keys are flagged inline so
 * the autosave doesn't persist a broken map (the Rust side would log a
 * warn + skip the entry, but the user wouldn't see why).
 */
export function CustomShortcutsCard() {
  const settings = useSettingsStore((s) => s.settings);
  const update = useSettingsStore((s) => s.update);
  // Local editor copy keyed by an array of (id, accel) tuples — the
  // Record<string,string> shape can't represent "two rows mid-edit
  // both blank". Same trick the Recorded apps card uses.
  const storeShortcuts = settings?.shortcuts;
  const [editor, setEditor] = useState<[string, string][]>(() =>
    Object.entries(storeShortcuts ?? {}),
  );
  const lastSyncedRef = useRef<Record<string, string> | null>(null);

  // Mirror an external store change (load, or another tab editing
  // the same field) into the editor copy. Reference-equal incoming
  // values skip the reset so an in-flight save doesn't clobber the
  // user's mid-edit rows.
  useEffect(() => {
    if (storeShortcuts === undefined) return;
    if (storeShortcuts === lastSyncedRef.current) return;
    setEditor(Object.entries(storeShortcuts));
    lastSyncedRef.current = storeShortcuts;
  }, [storeShortcuts]);

  if (settings === null) return null;

  function commit(next: [string, string][]) {
    setEditor(next);
    const ids = next.map(([id]) => id.trim());
    const accels = next.map(([, accel]) => accel.trim());
    const hasEmpty = ids.some((id) => id === "");
    const hasDupe = ids.length !== new Set(ids).size;
    // Empty accelerators are "in-progress" rows — don't promote them to
    // the store. `addRow(v)` seeds `[v, ""]`, and clearing an existing
    // accelerator passes through `setRow`; in both cases persisting the
    // half-written row would temporarily wipe a working shortcut.
    const hasEmptyAccel = accels.some((a) => a === "");
    if (hasEmpty || hasDupe || hasEmptyAccel) return;
    const map: Record<string, string> = {};
    for (const [id, accel] of next) {
      map[id.trim()] = accel.trim();
    }
    lastSyncedRef.current = map;
    update({ shortcuts: map });
  }

  function setRow(idx: number, field: 0 | 1, value: string) {
    const next = editor.slice();
    const [id, accel] = next[idx] ?? ["", ""];
    next[idx] = field === 0 ? [value, accel] : [id, value];
    commit(next);
  }

  function removeRow(idx: number) {
    const next = editor.slice();
    next.splice(idx, 1);
    commit(next);
  }

  function addRow(actionId: string) {
    commit([...editor, [actionId, ""]]);
  }

  const ids = editor.map(([id]) => id.trim());
  const hasEmpty = ids.some((id) => id === "");
  const hasDupe = ids.length !== new Set(ids).size;

  return (
    <div className="rounded-md border border-border p-4 space-y-3">
      <div className="text-sm font-medium">Custom shortcuts</div>
      <p className="text-xs text-muted-foreground">
        Tier 4 (PR #164). Bind action ids to accelerators. The renderer
        listens on <code>shortcut:&lt;action_id&gt;</code> events; an
        entry for <code>toggle_recording</code> overrides the default
        chord above.
      </p>

      {editor.length === 0 ? (
        <p className="text-xs text-muted-foreground italic">
          No custom shortcuts. The default record chord above is still
          active.
        </p>
      ) : (
        <div className="space-y-2">
          {editor.map(([id, accel], idx) => (
            <div
              // eslint-disable-next-line react/no-array-index-key
              key={idx}
              className="grid grid-cols-[1fr_1fr_auto] items-center gap-2"
            >
              <Input
                value={id}
                onChange={(e) => setRow(idx, 0, e.target.value)}
                placeholder="action_id"
                className="font-mono"
                aria-label={`Action id ${idx + 1}`}
              />
              <Input
                value={accel}
                onChange={(e) => setRow(idx, 1, e.target.value)}
                placeholder="CmdOrCtrl+Shift+R"
                className="font-mono"
                aria-label={`Accelerator ${idx + 1}`}
              />
              <Button
                variant="ghost"
                size="icon"
                onClick={() => removeRow(idx)}
                aria-label={`Remove ${id || "row"}`}
                title="Remove"
              >
                <Trash2 className="h-4 w-4" aria-hidden="true" />
              </Button>
            </div>
          ))}
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <Button
          variant="outline"
          size="sm"
          onClick={() => addRow("")}
          aria-label="Add shortcut"
        >
          <Plus className="h-3.5 w-3.5" aria-hidden="true" />
          Add shortcut
        </Button>
        <span className="text-xs text-muted-foreground">Quick add:</span>
        <select
          aria-label="Quick-add known action id"
          className={
            "h-8 rounded-md border border-border bg-background px-2 " +
            "text-xs focus-visible:outline-none focus-visible:ring-2 " +
            "focus-visible:ring-primary"
          }
          value=""
          onChange={(e) => {
            const v = e.target.value;
            if (v === "") return;
            if (!ids.includes(v)) {
              addRow(v);
            }
            // Reset selection — `value=""` is the controlled placeholder.
            e.currentTarget.value = "";
          }}
        >
          <option value="">Choose…</option>
          {KNOWN_SHORTCUT_ACTIONS.map((a) => (
            <option key={a.value} value={a.value}>
              {a.label}
            </option>
          ))}
        </select>
      </div>

      {(hasEmpty || hasDupe) && (
        <p className="text-xs text-destructive">
          {hasEmpty && "Action ids must not be empty."}
          {hasEmpty && hasDupe && " "}
          {hasDupe && "Each action id must be unique."}
        </p>
      )}
    </div>
  );
}
