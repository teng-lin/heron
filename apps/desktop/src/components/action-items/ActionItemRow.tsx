import type { ActionItemRow as ActionItemRowData } from "../../pages/Review";
import { formatActionItemDue } from "../../pages/Review";
import type { ActionItemEditController } from "./edit-controller";
import { isStableActionItemId, isValidIsoDate } from "./validation";

export type EditTarget = "text" | "owner" | "due";

export interface RowEditState {
  itemId: string;
  field: EditTarget;
  value: string;
  error: string | null;
}

const CHIP_CLASS =
  "inline-flex items-center rounded border px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-[0.06em]";
const CHIP_STYLE = {
  background: "var(--color-paper-2)",
  borderColor: "var(--color-rule)",
  color: "var(--color-ink-2)",
} as const;

interface ActionItemRowProps {
  item: ActionItemRowData;
  controller: ActionItemEditController;
  edit: RowEditState | null;
  setEdit: (next: RowEditState | null) => void;
}

/**
 * One row in the editor list. Owns the per-row read of the current
 * `edit` state and dispatches commits back to the shared controller.
 *
 * Behavioral notes:
 *  - `editable` requires BOTH a structured-wire row AND a UUID-shaped
 *    id. The controller gates the IPC on the same predicate; rendering
 *    matches so a non-writable row never shows a clickable checkbox /
 *    chip in the first place.
 *  - The due-edit input mirrors the text/owner inputs in committing
 *    on blur. `commit` re-validates the format, so a bad input still
 *    surfaces the inline alert instead of firing a rejected IPC. Per
 *    CodeRabbit on PR #180.
 */
export function ActionItemRow({
  item,
  controller,
  edit,
  setEdit,
}: ActionItemRowProps) {
  const isEditingText =
    edit !== null && edit.itemId === item.id && edit.field === "text";
  const isEditingOwner =
    edit !== null && edit.itemId === item.id && edit.field === "owner";
  const isEditingDue =
    edit !== null && edit.itemId === item.id && edit.field === "due";
  const editable = item.structured && isStableActionItemId(item.id);

  function startEdit(field: EditTarget, current: string) {
    setEdit({ itemId: item.id, field, value: current, error: null });
  }
  function cancelEdit() {
    setEdit(null);
  }

  async function commit(target: RowEditState) {
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
    <li className="flex items-start gap-2">
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
                void commit(edit);
              } else if (e.key === "Escape") {
                e.preventDefault();
                cancelEdit();
              }
            }}
            onBlur={() => {
              void commit(edit);
            }}
            aria-label="Edit action-item text"
            className="min-w-0 flex-1 rounded border bg-transparent px-1 py-0.5 text-sm"
            style={{ borderColor: "var(--color-rule)" }}
          />
        ) : (
          <span
            className={editable ? "cursor-text" : ""}
            style={
              item.done
                ? { textDecoration: "line-through", opacity: 0.6 }
                : undefined
            }
            onClick={
              editable ? () => startEdit("text", item.text) : undefined
            }
            role={editable ? "button" : undefined}
            tabIndex={editable ? 0 : undefined}
            onKeyDown={
              editable
                ? (e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      startEdit("text", item.text);
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
                void commit(edit);
              } else if (e.key === "Escape") {
                e.preventDefault();
                cancelEdit();
              }
            }}
            onBlur={() => {
              void commit(edit);
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
              editable ? () => startEdit("owner", item.owner ?? "") : undefined
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
            onClick={() => startEdit("owner", "")}
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
                  void commit(edit);
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  cancelEdit();
                }
              }}
              onBlur={() => {
                void commit(edit);
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
              editable ? () => startEdit("due", item.due ?? "") : undefined
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
            onClick={() => startEdit("due", "")}
          >
            + due
          </button>
        ) : null}
      </div>
    </li>
  );
}
