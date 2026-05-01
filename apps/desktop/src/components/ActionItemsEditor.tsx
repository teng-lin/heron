/**
 * Day 8–10 (action-item write-back) — editor surface for the Review
 * page's `Actions` tab.
 *
 * Built on top of the read-only rendering #177 landed: mirrors the same
 * Tailwind chip classes (`var(--color-paper-2)` / `var(--color-rule)`)
 * so the visual stays consistent. Affordances:
 *
 *   1. Checkbox flipping `done` on each `structured: true` row.
 *   2. Click-to-edit `text` inline (Enter commits, Esc cancels).
 *   3. Click-to-edit `owner` chip; null-owner rows render an `+ assignee`
 *      affordance instead. Empty submission clears the field.
 *   4. Click-to-edit `due` chip with `YYYY-MM-DD` validation. Bad input
 *      surfaces an inline error and does not fire the IPC.
 *   5. `structured: false` (regex-fallback) rows render no edit
 *      affordances — no backing `id` to write to.
 *
 * UI state (which row is editing what) lives in this component's local
 * state — explicitly NOT promoted to a global store per the brief.
 *
 * Optimistic write path: flip the local row immediately, fire
 * `updateActionItem`, on rejection roll back the row and surface a
 * toast. The pure controller (`createActionItemEditController`) is
 * exported separately so the Bun-only test runner can drive it without
 * a DOM — there's no jsdom in this workspace (see
 * `store/salvage.test.ts` for the same pattern).
 *
 * Issue #195 split this file along three seams: pure validation
 * helpers (`./action-items/validation.ts`), the optimistic-edit
 * controller factory (`./action-items/edit-controller.ts`), and the
 * single-row renderer (`./action-items/ActionItemRow.tsx`). The
 * exports below are the same public surface the test file pins.
 */

import { useEffect, useMemo, useState } from "react";

import type { ActionItemRow as ActionItemRowData } from "../pages/Review";
import { ActionItemRow, type RowEditState } from "./action-items/ActionItemRow";
import { createActionItemEditController } from "./action-items/edit-controller";

// Re-exports preserved so existing callers and tests keep working
// after the split — `ActionItemsEditor.test.ts` imports these names
// from `./ActionItemsEditor` directly.
export {
  createActionItemEditController,
  type ActionItemEditController,
  type ActionItemEditDeps,
} from "./action-items/edit-controller";
export {
  isStableActionItemId,
  isValidIsoDate,
} from "./action-items/validation";

export interface ActionItemsEditorProps {
  rows: ActionItemRowData[];
  vaultPath: string;
  meetingId: string;
  onError?: (message: string) => void;
}

export function ActionItemsEditor({
  rows,
  vaultPath,
  meetingId,
  onError,
}: ActionItemsEditorProps) {
  // Re-build the controller whenever the upstream rows array identity
  // changes (e.g. a fresh `selectActionItems` invocation after the
  // meeting reload). Edits in flight on the previous controller are
  // dropped — by then the rows have been replaced anyway, so the
  // optimistic state would be stale.
  const controller = useMemo(
    () =>
      createActionItemEditController(rows, { vaultPath, meetingId, onError }),
    [rows, vaultPath, meetingId, onError],
  );

  // Subscribe via a counter so React re-renders when the controller
  // mutates its internal row list. `useSyncExternalStore` would also
  // work; the counter approach is one fewer import for the same
  // semantics in this small surface.
  const [, setVersion] = useState(0);
  useEffect(() => {
    return controller.subscribe(() => setVersion((v) => v + 1));
  }, [controller]);

  const [edit, setEdit] = useState<RowEditState | null>(null);

  const liveRows = controller.getRows();

  return (
    <ul className="space-y-2 text-sm">
      {liveRows.map((item) => (
        <ActionItemRow
          key={item.id}
          item={item}
          controller={controller}
          edit={edit}
          setEdit={setEdit}
        />
      ))}
    </ul>
  );
}
