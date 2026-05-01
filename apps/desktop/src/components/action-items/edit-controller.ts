/**
 * Optimistic-edit controller for the action-item write-back surface.
 *
 * Owns the row list and exposes optimistic-update operations. The
 * React component subscribes via a `version` counter that bumps on
 * every state change.
 *
 * Constructed once per (rows, deps) tuple — when the upstream rows
 * change (e.g. fresh load), the React component re-creates the
 * controller via `useMemo`. The factory pattern keeps the row-state
 * and the IPC plumbing testable without a renderer — the brief calls
 * for mocking `updateActionItem` at the import boundary, and that's
 * the `update` injection point.
 */

import { updateActionItem, type ActionItemPatch } from "../../lib/invoke";
import type { ActionItemRow } from "../../pages/Review";
import { isStableActionItemId, isValidIsoDate } from "./validation";

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

  // Per-(row, field-set) sequence number. Two in-flight edits on the
  // same field can complete out of order — without this, an older
  // failure rolling back AFTER a newer success leaves the UI showing
  // the old value while the vault has the new one. Each call records
  // its sequence, and rollback only fires if we're still the latest.
  // Per CodeRabbit on PR #180.
  let opSeq = 0;
  const latestOpByKey = new Map<string, number>();
  function opKey(itemId: string, patch: ActionItemPatch): string {
    return `${itemId}:${Object.keys(patch).sort().join(",")}`;
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
    const key = opKey(itemId, patch);
    const myOp = ++opSeq;
    latestOpByKey.set(key, myOp);
    patchRow(itemId, optimistic);
    try {
      await update({
        vaultPath: deps.vaultPath,
        meetingId: deps.meetingId,
        itemId,
        patch,
      });
      if (latestOpByKey.get(key) === myOp) {
        latestOpByKey.delete(key);
      }
      return true;
    } catch (err) {
      // Only roll back if no newer op for this (row, field-set) has
      // already been issued — the newer op's optimistic value should
      // win regardless of how it ultimately resolves.
      if (latestOpByKey.get(key) === myOp) {
        patchRow(itemId, rollback);
        latestOpByKey.delete(key);
      }
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
