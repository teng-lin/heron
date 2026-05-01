/**
 * `action-item-edit.spec.ts` — issue #217 smoke flow #5.
 *
 * Drives the `ActionItemsEditor` optimistic-edit path: tick the
 * checkbox on a structured action item, assert the row's checked
 * state flips before the IPC resolves, then pin the resulting
 * `heron_update_action_item` call's args.
 *
 * Why a `done` toggle and not a text edit? The text-edit path
 * commits on blur, which is hard to drive deterministically without
 * hitting Tab focus rules. A checkbox click exercises the same
 * `applyOptimistic` path through `controller.toggleDone`. The
 * unit-test suite (`ActionItemsEditor.test.ts`) covers per-field
 * permutations.
 */

import { expect, test } from "@playwright/test";

import { drainCalls, getCalls, mockIpc } from "./_fixture";

const SESSION_ID = "mtg_01jegedt-7000-0000-0000-000000000001";

const ACTION_ITEM_ID = "33333333-3333-7333-8333-333333333333";

const MEETING = {
  id: SESSION_ID,
  status: "done",
  platform: "zoom",
  title: "Action item edit smoke",
  calendar_event_id: null,
  started_at: "2026-04-30T12:00:00Z",
  ended_at: "2026-04-30T12:30:00Z",
  duration_secs: 1800,
  participants: [],
  transcript_status: "complete",
  summary_status: "ready",
  tags: [],
  action_items: [
    {
      id: ACTION_ITEM_ID,
      text: "Toggle me",
      owner: null,
      due: null,
      done: false,
    },
  ],
};

const NOTE_BODY = `# Action item edit smoke

## Action Items

- Toggle me
`;

test.describe("action item edit", () => {
  test("checkbox click fires heron_update_action_item with done: true", async ({
    page,
  }) => {
    await mockIpc(page, {
      heron_get_meeting: { kind: "ok", data: MEETING },
      heron_meeting_transcript: {
        kind: "ok",
        data: {
          meeting_id: SESSION_ID,
          status: "complete",
          language: "en",
          segments: [],
        },
      },
      heron_read_note: NOTE_BODY,
      heron_check_backup: null,
      // The Rust writer returns the post-merge `ActionItemView`. The
      // editor doesn't read the return value (the optimistic patch
      // already populated the row), but the promise must resolve, not
      // reject, or the controller hits the rollback branch.
      heron_update_action_item: {
        id: ACTION_ITEM_ID,
        text: "Toggle me",
        owner: null,
        due: null,
        done: true,
      },
    });

    await page.goto(`/review/${SESSION_ID}`);

    await expect(page.getByTitle("Action item edit smoke")).toBeVisible({
      timeout: 10_000,
    });

    await page.getByRole("tab", { name: /^actions$/i }).click();

    const checkbox = page.getByRole("checkbox", { name: /toggle me/i });
    await expect(checkbox).toBeVisible();
    await expect(checkbox).not.toBeChecked();

    await drainCalls(page);
    await checkbox.click();

    // Optimistic UI: the checkbox flips immediately, before the IPC
    // resolves. A regression that awaits the IPC before flipping
    // local state would still pass the IPC assertion below but fail
    // this one.
    await expect(checkbox).toBeChecked();

    await expect
      .poll(
        async () =>
          (await getCalls(page)).find(
            (c) => c.cmd === "heron_update_action_item",
          ),
        { timeout: 3_000 },
      )
      .toBeTruthy();

    const calls = await drainCalls(page);
    const call = calls.find((c) => c.cmd === "heron_update_action_item");
    expect(call).toBeDefined();
    const args = call!.args as {
      vaultPath: string;
      meetingId: string;
      itemId: string;
      patch: { done?: boolean };
    };
    expect(args.meetingId).toBe(SESSION_ID);
    expect(args.itemId).toBe(ACTION_ITEM_ID);
    expect(args.patch.done).toBe(true);
  });
});
