/**
 * Pure-controller tests for the action-item write-back surface.
 *
 * Bun-only test runner (no jsdom in this workspace — see
 * `store/salvage.test.ts` for the same pattern), so we exercise the
 * controller factory directly rather than mounting React. The
 * factory takes an injectable `update` so the IPC wrapper can be
 * mocked at the import boundary, which is the contract the brief
 * calls out.
 *
 * The controller is the load-bearing piece: it owns optimistic
 * updates, rollback on rejection, and the structured-row gating that
 * keeps regex-fallback rows from accidentally hitting the IPC. The
 * React component is a thin shell over it.
 *
 * Cases pinned:
 *
 *   1. Checkbox flip calls `updateActionItem` with `{ done: true }`.
 *   2. Owner edit Enter → `{ owner: "name" }`; clear → `{ owner: null }`.
 *   3. Due edit with bad format does not fire the IPC.
 *   4. `structured: false` rows render no edit affordances (controller
 *      ignores their write attempts even when called directly).
 *   5. Optimistic state rolls back on `updateActionItem` rejection.
 */

import { describe, expect, test } from "bun:test";

import {
  createActionItemEditController,
  isStableActionItemId,
  isValidIsoDate,
} from "./ActionItemsEditor";
import type { ActionItemRow } from "../pages/Review";
import type { ActionItemPatch } from "../lib/invoke";

// Real UUID — the controller now gates writes on UUID-shape so the
// fixture has to satisfy the production predicate.
const TEST_ITEM_ID = "550e8400-e29b-41d4-a716-446655440000";

interface UpdateCall {
  vaultPath: string;
  meetingId: string;
  itemId: string;
  patch: ActionItemPatch;
}

function makeUpdate(
  outcomes: Array<{ kind: "ok" } | { kind: "err"; message: string }> = [],
) {
  const calls: UpdateCall[] = [];
  let i = 0;
  const update = async (args: {
    vaultPath: string;
    meetingId: string;
    itemId: string;
    patch: ActionItemPatch;
  }) => {
    calls.push(args);
    const outcome = outcomes[i++] ?? { kind: "ok" };
    if (outcome.kind === "err") {
      throw new Error(outcome.message);
    }
    return undefined as unknown;
  };
  return { update, calls };
}

function row(overrides: Partial<ActionItemRow> = {}): ActionItemRow {
  return {
    id: TEST_ITEM_ID,
    text: "Write the doc",
    owner: null,
    due: null,
    done: false,
    structured: true,
    ...overrides,
  };
}

describe("isValidIsoDate", () => {
  test("accepts well-formed YYYY-MM-DD", () => {
    expect(isValidIsoDate("2026-05-01")).toBe(true);
    expect(isValidIsoDate("2026-12-31")).toBe(true);
  });

  test("rejects shapes that don't match the anchored regex", () => {
    expect(isValidIsoDate("2026/05/01")).toBe(false);
    expect(isValidIsoDate("2026-5-1")).toBe(false);
    expect(isValidIsoDate("not a date")).toBe(false);
    expect(isValidIsoDate("2026-05-01T00:00:00Z")).toBe(false);
    expect(isValidIsoDate(" 2026-05-01")).toBe(false);
  });

  test("rejects regex-shaped but calendar-impossible dates", () => {
    // Mirrors `chrono::NaiveDate::parse_from_str("%Y-%m-%d")` semantics
    // — the Rust writer would reject these; the TS gate catches them
    // before they round-trip through optimistic UI.
    expect(isValidIsoDate("2026-13-01")).toBe(false);
    expect(isValidIsoDate("2026-02-30")).toBe(false);
    expect(isValidIsoDate("2026-04-31")).toBe(false);
    expect(isValidIsoDate("9999-99-99")).toBe(false);
    expect(isValidIsoDate("2026-00-15")).toBe(false);
    expect(isValidIsoDate("2026-12-00")).toBe(false);
  });
});

describe("isStableActionItemId", () => {
  test("accepts canonical UUIDs", () => {
    expect(isStableActionItemId("550e8400-e29b-41d4-a716-446655440000")).toBe(
      true,
    );
    expect(isStableActionItemId("00000000-0000-0000-0000-000000000000")).toBe(
      true,
    );
  });

  test("rejects synthesized prefixes from selectActionItems", () => {
    expect(isStableActionItemId("legacy:0")).toBe(false);
    expect(isStableActionItemId("legacy:42")).toBe(false);
    expect(isStableActionItemId("fallback:0")).toBe(false);
    expect(isStableActionItemId("item-1")).toBe(false);
    expect(isStableActionItemId("")).toBe(false);
  });
});

describe("createActionItemEditController", () => {
  test("toggleDone flips the row and fires update with { done: true }", async () => {
    const { update, calls } = makeUpdate();
    const c = createActionItemEditController([row()], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    await c.toggleDone(TEST_ITEM_ID, true);
    expect(c.getRows()[0].done).toBe(true);
    expect(calls).toHaveLength(1);
    expect(calls[0]).toEqual({
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      itemId: TEST_ITEM_ID,
      patch: { done: true },
    });
    // No leftover keys — exactly the four-arg shape.
    expect(Object.keys(calls[0]).sort()).toEqual([
      "itemId",
      "meetingId",
      "patch",
      "vaultPath",
    ]);
  });

  test("toggleDone is a no-op when value is unchanged", async () => {
    const { update, calls } = makeUpdate();
    const c = createActionItemEditController([row({ done: false })], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    await c.toggleDone(TEST_ITEM_ID, false);
    expect(calls).toHaveLength(0);
  });

  test("commitOwner with a name fires { owner: 'name' }", async () => {
    const { update, calls } = makeUpdate();
    const c = createActionItemEditController([row()], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    await c.commitOwner(TEST_ITEM_ID, "Teng");
    expect(c.getRows()[0].owner).toBe("Teng");
    expect(calls[0].patch).toEqual({ owner: "Teng" });
  });

  test("commitOwner with empty string clears the field", async () => {
    const { update, calls } = makeUpdate();
    const c = createActionItemEditController([row({ owner: "Teng" })], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    await c.commitOwner(TEST_ITEM_ID, "   ");
    expect(c.getRows()[0].owner).toBeNull();
    expect(calls[0].patch).toEqual({ owner: null });
  });

  test("commitDue with bad format returns false and does not fire IPC", async () => {
    const { update, calls } = makeUpdate();
    const c = createActionItemEditController([row()], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    const ok = await c.commitDue(TEST_ITEM_ID, "tomorrow");
    expect(ok).toBe(false);
    expect(calls).toHaveLength(0);
    expect(c.getRows()[0].due).toBeNull();
  });

  test("commitDue with valid YYYY-MM-DD fires { due: ... }", async () => {
    const { update, calls } = makeUpdate();
    const c = createActionItemEditController([row()], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    const ok = await c.commitDue(TEST_ITEM_ID, "2026-05-01");
    expect(ok).toBe(true);
    expect(c.getRows()[0].due).toBe("2026-05-01");
    expect(calls[0].patch).toEqual({ due: "2026-05-01" });
  });

  test("commitDue with empty string clears the field", async () => {
    const { update, calls } = makeUpdate();
    const c = createActionItemEditController([row({ due: "2026-05-01" })], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    const ok = await c.commitDue(TEST_ITEM_ID, "");
    expect(ok).toBe(true);
    expect(c.getRows()[0].due).toBeNull();
    expect(calls[0].patch).toEqual({ due: null });
  });

  test("commitText updates the row text and fires { text }", async () => {
    const { update, calls } = makeUpdate();
    const c = createActionItemEditController([row()], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    await c.commitText(TEST_ITEM_ID, "Write the better doc");
    expect(c.getRows()[0].text).toBe("Write the better doc");
    expect(calls[0].patch).toEqual({ text: "Write the better doc" });
  });

  test("commitText is a no-op on an empty / unchanged input", async () => {
    const { update, calls } = makeUpdate();
    const c = createActionItemEditController([row()], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    await c.commitText(TEST_ITEM_ID, "  ");
    await c.commitText(TEST_ITEM_ID, "Write the doc");
    expect(calls).toHaveLength(0);
  });

  test("structured: false rows reject every write attempt", async () => {
    // Belt-and-suspenders alongside the React component's affordance
    // gating: even if a caller hands the controller a fallback row's
    // id, the controller refuses to fire the IPC.
    const { update, calls } = makeUpdate();
    const fallback = row({
      id: "fallback:0",
      structured: false,
      done: false,
    });
    const c = createActionItemEditController([fallback], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    await c.toggleDone("fallback:0", true);
    await c.commitText("fallback:0", "edited");
    await c.commitOwner("fallback:0", "Teng");
    expect(await c.commitDue("fallback:0", "2026-05-01")).toBe(false);
    expect(calls).toHaveLength(0);
    expect(c.getRows()[0]).toEqual(fallback);
  });

  test("structured: true but non-UUID id (legacy:N) is gated at the controller", async () => {
    // `selectActionItems` synthesizes `legacy:<idx>` for structured-wire
    // rows whose `ActionItem.id` was dropped by a pre-Tier-0-#3 daemon.
    // The Rust writer parses `item_id` as a UUID and rejects synthesized
    // prefixes with a confusing `validation:` toast — gate at the
    // controller so the optimistic UI never fires the IPC in the first
    // place. Mirrors the React surface's `editable` predicate.
    const { update, calls } = makeUpdate();
    const legacy = row({
      id: "legacy:0",
      structured: true,
      done: false,
    });
    const c = createActionItemEditController([legacy], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    await c.toggleDone("legacy:0", true);
    await c.commitText("legacy:0", "edited");
    await c.commitOwner("legacy:0", "Teng");
    expect(await c.commitDue("legacy:0", "2026-05-01")).toBe(false);
    expect(calls).toHaveLength(0);
    // No optimistic mutation either — the row's `done`/`text`/etc.
    // stay at their seeded values since `applyOptimistic` returns
    // before `patchRow`.
    expect(c.getRows()[0]).toEqual(legacy);
  });

  test("rolls back optimistic state on update rejection and surfaces error", async () => {
    const { update, calls } = makeUpdate([
      { kind: "err", message: "daemon unavailable" },
    ]);
    const errors: string[] = [];
    const c = createActionItemEditController([row({ done: false })], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
      onError: (m) => errors.push(m),
    });
    await c.toggleDone(TEST_ITEM_ID, true);
    expect(calls).toHaveLength(1);
    // Rolled back to the prior state.
    expect(c.getRows()[0].done).toBe(false);
    expect(errors).toHaveLength(1);
    expect(errors[0]).toContain("daemon unavailable");
  });

  test("rolls back owner edit on rejection", async () => {
    const { update, calls } = makeUpdate([
      { kind: "err", message: "boom" },
    ]);
    const c = createActionItemEditController([row({ owner: "Teng" })], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
      onError: () => {},
    });
    await c.commitOwner(TEST_ITEM_ID, "Other");
    expect(calls).toHaveLength(1);
    expect(c.getRows()[0].owner).toBe("Teng");
  });

  test("stale rollback does not clobber a newer in-flight success", async () => {
    // Per CodeRabbit on PR #180: two edits on the same row + field can
    // complete out of order. An older request failing AFTER a newer
    // request succeeds must not roll back the newer value — otherwise
    // the UI shows the stale value while the vault has the new one.
    //
    // We script the update to: first call rejects (stale), second call
    // resolves (current). We hand back manual control over each call
    // via deferred promises so we can sequence the resolution order.
    let resolveCall1!: () => void;
    let rejectCall1!: (err: Error) => void;
    let resolveCall2!: () => void;
    const calls: Array<{ patch: ActionItemPatch }> = [];
    let i = 0;
    const update = async (args: {
      vaultPath: string;
      meetingId: string;
      itemId: string;
      patch: ActionItemPatch;
    }) => {
      calls.push({ patch: args.patch });
      const idx = i++;
      return new Promise<unknown>((resolve, reject) => {
        if (idx === 0) {
          rejectCall1 = reject;
          resolveCall1 = () => resolve(undefined);
        } else {
          resolveCall2 = resolve as () => void;
        }
      });
    };
    const errors: string[] = [];
    const c = createActionItemEditController([row({ owner: null })], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
      onError: (m) => errors.push(m),
    });

    // Issue two owner edits on the same row.
    const p1 = c.commitOwner(TEST_ITEM_ID, "alice");
    const p2 = c.commitOwner(TEST_ITEM_ID, "bob");

    // Optimistic state reflects the LATEST op (bob).
    expect(c.getRows()[0].owner).toBe("bob");

    // The newer (second) call resolves first.
    resolveCall2();
    await p2;

    // Then the older (first) call rejects — its rollback would set
    // owner back to null. The sequence guard MUST prevent that.
    rejectCall1(new Error("daemon hiccup"));
    await p1;

    expect(c.getRows()[0].owner).toBe("bob");
    // The error toast still fires for the failed op.
    expect(errors).toHaveLength(1);
    // The reverse case (older succeeds, newer rejects) rolls back
    // correctly because the newer op IS the latest by sequence.
    void resolveCall1;
  });

  test("subscribe fires on every state change", async () => {
    const { update } = makeUpdate();
    const c = createActionItemEditController([row()], {
      vaultPath: "/test/vault",
      meetingId: "mtg_test",
      update,
    });
    let fired = 0;
    const unsub = c.subscribe(() => {
      fired++;
    });
    await c.toggleDone(TEST_ITEM_ID, true);
    // One notify for the optimistic flip; success path needs no
    // second flip because the optimistic value already matches.
    expect(fired).toBeGreaterThanOrEqual(1);
    unsub();
    const before = fired;
    await c.toggleDone(TEST_ITEM_ID, false);
    expect(fired).toBe(before);
  });
});
