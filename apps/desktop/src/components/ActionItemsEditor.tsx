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
 */

import { useEffect, useMemo, useState } from "react";

import { updateActionItem, type ActionItemPatch } from "../lib/invoke";
import type { ActionItemRow } from "../pages/Review";
import { formatActionItemDue } from "../pages/Review";

const ISO_DATE_RE = /^\d{4}-\d{2}-\d{2}$/;
const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * `YYYY-MM-DD` calendar-date check. The Rust side validates via
 * `chrono::NaiveDate::parse_from_str` which rejects nonsense like
 * `2026-13-01` or `2026-02-30`; we mirror that semantic here so a
 * laxer client doesn't round-trip a bad date through optimistic UI
 * just to be rejected at the writer boundary. The shape regex catches
 * `9999-99-99`-style inputs at the keystroke; the `Date` round-trip
 * catches the calendar-impossible cases the regex admits.
 *
 * Exported for the controller test — the validation is the load-bearing
 * piece of the due-edit flow.
 */
export function isValidIsoDate(value: string): boolean {
  if (!ISO_DATE_RE.test(value)) return false;
  const parsed = new Date(`${value}T00:00:00Z`);
  return !Number.isNaN(parsed.getTime()) && parsed.toISOString().startsWith(value);
}

/**
 * `true` when `id` is a real UUID — the only id shape the Rust
 * `update_action_item` boundary accepts. Synthesized prefixes from
 * `selectActionItems` (`legacy:N` for structured rows whose wire id
 * was dropped by a pre-Tier-0-#3 daemon, `fallback:N` for regex-
 * extracted bullets) deliberately fail this check so the optimistic
 * UI doesn't fire an IPC the backend will reject with a confusing
 * "validation: item_id is not a UUID" toast.
 *
 * Exported so the React surface can branch its affordances on the
 * same predicate the controller uses to gate IPC calls — a row that
 * fails this check renders without checkbox / edit chips so the
 * user can't even click. (Belt-and-suspenders: the controller still
 * gates internally so a direct caller can't bypass.)
 */
export function isStableActionItemId(id: string): boolean {
  return UUID_RE.test(id);
}

/**
 * Pure controller backing the editor. Holds the current row list and
 * exposes optimistic-update operations; the React component subscribes
 * via a `version` counter that bumps on every state change.
 *
 * Constructed once per (rows, deps) tuple — when the upstream rows
 * change (e.g. fresh load), the React component re-creates the
 * controller via `useMemo`.
 */
export interface ActionItemEditController {
  /** Snapshot of the current rows. */
  getRows(): ActionItemRow[];
  /** Subscribe to state changes; returns an unsubscribe fn. */
  subscribe(listener: () => void): () => void;
  /** Optimistic toggle of `done`; rolls back on IPC rejection. */
  toggleDone(itemId: string, next: boolean): Promise<void>;
  /** Commit a new `text` value (no-op if unchanged). */
  commitText(itemId: string, next: string): Promise<void>;
  /**
   * Commit a new `owner` value. An empty / whitespace-only `next`
   * clears the field (`owner: null`).
   */
  commitOwner(itemId: string, next: string): Promise<void>;
  /**
   * Commit a new `due` value. Pre-validated by the caller; the
   * controller still defends against bad input by rejecting non-ISO
   * shapes (returns `false` without firing the IPC). An empty /
   * whitespace-only `next` clears the field (`due: null`).
   */
  commitDue(itemId: string, next: string): Promise<boolean>;
}

export interface ActionItemEditDeps {
  vaultPath: string;
  meetingId: string;
  /** Defaults to the real `updateActionItem`; injectable for tests. */
  update?: (args: {
    vaultPath: string;
    meetingId: string;
    itemId: string;
    patch: ActionItemPatch;
  }) => Promise<unknown>;
  /** Optional error sink; defaults to no-op. UI passes a Sonner toast. */
  onError?: (message: string) => void;
}

/**
 * Build a controller. The factory pattern keeps the row-state and the
 * IPC plumbing testable without a renderer — the brief calls for
 * mocking `updateActionItem` at the import boundary, and that's the
 * `update` injection point.
 */
export function createActionItemEditController(
  initial: ActionItemRow[],
  deps: ActionItemEditDeps,
): ActionItemEditController {
  const update = deps.update ?? updateActionItem;
  const onError = deps.onError ?? (() => {});
  let rows: ActionItemRow[] = initial.map((r) => ({ ...r }));
  const listeners = new Set<() => void>();

  function notify() {
    for (const l of listeners) l();
  }

  function patchRow(itemId: string, patch: Partial<ActionItemRow>) {
    rows = rows.map((r) => (r.id === itemId ? { ...r, ...patch } : r));
    notify();
  }

  function findRow(itemId: string): ActionItemRow | undefined {
    return rows.find((r) => r.id === itemId);
  }

  async function applyOptimistic(
    itemId: string,
    optimistic: Partial<ActionItemRow>,
    rollback: Partial<ActionItemRow>,
    patch: ActionItemPatch,
    errorPrefix: string,
  ): Promise<boolean> {
    const target = findRow(itemId);
    if (!target || !target.structured || !isStableActionItemId(itemId)) {
      return false;
    }
    patchRow(itemId, optimistic);
    try {
      await update({
        vaultPath: deps.vaultPath,
        meetingId: deps.meetingId,
        itemId,
        patch,
      });
      return true;
    } catch (err) {
      patchRow(itemId, rollback);
      const message = err instanceof Error ? err.message : String(err);
      onError(`${errorPrefix}: ${message}`);
      return false;
    }
  }

  return {
    getRows() {
      return rows;
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    async toggleDone(itemId, next) {
      const target = findRow(itemId);
      if (!target || !target.structured) return;
      if (target.done === next) return;
      await applyOptimistic(
        itemId,
        { done: next },
        { done: target.done },
        { done: next },
        "Failed to update action item",
      );
    },
    async commitText(itemId, next) {
      const target = findRow(itemId);
      if (!target || !target.structured) return;
      const trimmed = next.trim();
      if (trimmed.length === 0 || trimmed === target.text) return;
      await applyOptimistic(
        itemId,
        { text: trimmed },
        { text: target.text },
        { text: trimmed },
        "Failed to update action item",
      );
    },
    async commitOwner(itemId, next) {
      const target = findRow(itemId);
      if (!target || !target.structured) return;
      const trimmed = next.trim();
      if (trimmed.length === 0) {
        if (target.owner === null) return;
        await applyOptimistic(
          itemId,
          { owner: null },
          { owner: target.owner },
          { owner: null },
          "Failed to clear owner",
        );
        return;
      }
      if (trimmed === target.owner) return;
      await applyOptimistic(
        itemId,
        { owner: trimmed },
        { owner: target.owner },
        { owner: trimmed },
        "Failed to update owner",
      );
    },
    async commitDue(itemId, next) {
      const target = findRow(itemId);
      if (!target || !target.structured) return false;
      const trimmed = next.trim();
      if (trimmed.length === 0) {
        if (target.due === null) return true;
        return applyOptimistic(
          itemId,
          { due: null },
          { due: target.due },
          { due: null },
          "Failed to clear due date",
        );
      }
      if (!isValidIsoDate(trimmed)) return false;
      if (trimmed === target.due) return true;
      return applyOptimistic(
        itemId,
        { due: trimmed },
        { due: target.due },
        { due: trimmed },
        "Failed to update due date",
      );
    },
  };
}

// ---- React surface ------------------------------------------------

const CHIP_CLASS =
  "inline-flex items-center rounded border px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-[0.06em]";
const CHIP_STYLE = {
  background: "var(--color-paper-2)",
  borderColor: "var(--color-rule)",
  color: "var(--color-ink-2)",
} as const;

type EditTarget = "text" | "owner" | "due";

interface RowEditState {
  itemId: string;
  field: EditTarget;
  value: string;
  error: string | null;
}

export interface ActionItemsEditorProps {
  rows: ActionItemRow[];
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

  function startEdit(itemId: string, field: EditTarget, current: string) {
    setEdit({ itemId, field, value: current, error: null });
  }
  function cancelEdit() {
    setEdit(null);
  }

  async function commit(item: ActionItemRow, target: RowEditState) {
    const value = target.value;
    if (target.field === "text") {
      await controller.commitText(item.id, value);
      setEdit(null);
      return;
    }
    if (target.field === "owner") {
      await controller.commitOwner(item.id, value);
      setEdit(null);
      return;
    }
    // due — `commitDue` handles the empty-string clear path internally.
    const trimmed = value.trim();
    if (trimmed.length > 0 && !isValidIsoDate(trimmed)) {
      setEdit({ ...target, error: "Use YYYY-MM-DD" });
      return;
    }
    const ok = await controller.commitDue(item.id, trimmed);
    if (ok) setEdit(null);
  }

  return (
    <ul className="space-y-2 text-sm">
      {liveRows.map((item) => {
        const isEditingText =
          edit !== null && edit.itemId === item.id && edit.field === "text";
        const isEditingOwner =
          edit !== null && edit.itemId === item.id && edit.field === "owner";
        const isEditingDue =
          edit !== null && edit.itemId === item.id && edit.field === "due";
        // Edit affordances require BOTH a structured-wire row AND a
        // UUID-shaped id. The controller gates the IPC on the same
        // predicate; rendering matches so a non-writable row never
        // shows a clickable checkbox / chip in the first place.
        const editable = item.structured && isStableActionItemId(item.id);

        return (
          <li key={item.id} className="flex items-start gap-2">
            {editable ? (
              <input
                type="checkbox"
                checked={item.done}
                onChange={(e) => {
                  void controller.toggleDone(item.id, e.target.checked);
                }}
                aria-label={`Mark "${item.text}" done`}
                className="mt-1"
              />
            ) : (
              <span aria-hidden="true" className="mt-1 select-none">
                •
              </span>
            )}

            <div className="flex min-w-0 flex-1 flex-wrap items-center gap-1.5">
              {isEditingText && edit ? (
                <input
                  type="text"
                  autoFocus
                  value={edit.value}
                  onChange={(e) =>
                    setEdit({ ...edit, value: e.target.value, error: null })
                  }
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      void commit(item, edit);
                    } else if (e.key === "Escape") {
                      e.preventDefault();
                      cancelEdit();
                    }
                  }}
                  onBlur={() => {
                    void commit(item, edit);
                  }}
                  aria-label="Edit action-item text"
                  className="min-w-0 flex-1 rounded border bg-transparent px-1 py-0.5 text-sm"
                  style={{ borderColor: "var(--color-rule)" }}
                />
              ) : (
                <span
                  className={
                    editable
                      ? "cursor-text"
                      : ""
                  }
                  style={
                    item.done
                      ? { textDecoration: "line-through", opacity: 0.6 }
                      : undefined
                  }
                  onClick={
                    editable
                      ? () => startEdit(item.id, "text", item.text)
                      : undefined
                  }
                  role={editable ? "button" : undefined}
                  tabIndex={editable ? 0 : undefined}
                  onKeyDown={
                    editable
                      ? (e) => {
                          if (e.key === "Enter" || e.key === " ") {
                            e.preventDefault();
                            startEdit(item.id, "text", item.text);
                          }
                        }
                      : undefined
                  }
                >
                  {item.text}
                </span>
              )}

              {editable && isEditingOwner && edit ? (
                <input
                  type="text"
                  autoFocus
                  value={edit.value}
                  onChange={(e) =>
                    setEdit({ ...edit, value: e.target.value, error: null })
                  }
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      void commit(item, edit);
                    } else if (e.key === "Escape") {
                      e.preventDefault();
                      cancelEdit();
                    }
                  }}
                  onBlur={() => {
                    void commit(item, edit);
                  }}
                  aria-label="Edit owner"
                  placeholder="owner"
                  className={`${CHIP_CLASS} bg-transparent`}
                  style={{
                    ...CHIP_STYLE,
                    minWidth: "6rem",
                  }}
                />
              ) : item.owner !== null ? (
                <button
                  type="button"
                  className={CHIP_CLASS}
                  style={CHIP_STYLE}
                  title="Owner — click to edit"
                  onClick={
                    editable
                      ? () => startEdit(item.id, "owner", item.owner ?? "")
                      : undefined
                  }
                  disabled={!editable}
                >
                  {item.owner}
                </button>
              ) : editable ? (
                <button
                  type="button"
                  className={CHIP_CLASS}
                  style={{ ...CHIP_STYLE, color: "var(--color-ink-3)" }}
                  title="Add an assignee"
                  onClick={() => startEdit(item.id, "owner", "")}
                >
                  + assignee
                </button>
              ) : null}

              {editable && isEditingDue && edit ? (
                <span className="inline-flex flex-col">
                  <input
                    type="text"
                    autoFocus
                    value={edit.value}
                    onChange={(e) =>
                      setEdit({
                        ...edit,
                        value: e.target.value,
                        error: null,
                      })
                    }
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        e.preventDefault();
                        void commit(item, edit);
                      } else if (e.key === "Escape") {
                        e.preventDefault();
                        cancelEdit();
                      }
                    }}
                    aria-label="Edit due date"
                    placeholder="YYYY-MM-DD"
                    className={`${CHIP_CLASS} bg-transparent`}
                    style={{
                      ...CHIP_STYLE,
                      borderColor: edit.error
                        ? "var(--color-warn)"
                        : "var(--color-rule)",
                      minWidth: "8rem",
                    }}
                  />
                  {edit.error && (
                    <span
                      role="alert"
                      className="mt-0.5 text-[10px]"
                      style={{ color: "var(--color-warn)" }}
                    >
                      {edit.error}
                    </span>
                  )}
                </span>
              ) : item.due !== null ? (
                <button
                  type="button"
                  className={CHIP_CLASS}
                  style={CHIP_STYLE}
                  title={`Due ${item.due} — click to edit`}
                  onClick={
                    editable
                      ? () => startEdit(item.id, "due", item.due ?? "")
                      : undefined
                  }
                  disabled={!editable}
                >
                  due {formatActionItemDue(item.due)}
                </button>
              ) : editable ? (
                <button
                  type="button"
                  className={CHIP_CLASS}
                  style={{ ...CHIP_STYLE, color: "var(--color-ink-3)" }}
                  title="Add a due date"
                  onClick={() => startEdit(item.id, "due", "")}
                >
                  + due
                </button>
              ) : null}
            </div>
          </li>
        );
      })}
    </ul>
  );
}
