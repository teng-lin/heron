/**
 * `review-rail.spec.ts` — issue #217 smoke flow #3.
 *
 * Loads `/review/:sessionId` end-to-end (`heron_get_meeting` +
 * `heron_read_note`), navigates to the Actions tab, and asserts the
 * structured action-item rows render with owner + due chips.
 */

import { expect, test } from "@playwright/test";

import { getCalls, mockIpc } from "./_fixture";

const SESSION_ID = "mtg_01jegrev-7000-0000-0000-000000000001";

const ACTION_ITEM_ID = "11111111-1111-7111-8111-111111111111";

const MEETING = {
  id: SESSION_ID,
  status: "done",
  platform: "zoom",
  title: "Smoke review",
  calendar_event_id: null,
  started_at: "2026-04-30T12:00:00Z",
  ended_at: "2026-04-30T12:30:00Z",
  duration_secs: 1800,
  participants: [],
  transcript_status: "complete",
  summary_status: "ready",
  tags: ["e2e"],
  action_items: [
    {
      id: ACTION_ITEM_ID,
      text: "Ship the e2e smoke specs",
      owner: "tenglin",
      due: "2026-05-15",
      done: false,
    },
  ],
  processing: {
    summary_usd: 0.0123,
    tokens_in: 1024,
    tokens_out: 512,
    model: "claude-sonnet-4-6",
  },
};

const NOTE_BODY = `# Smoke review

A short summary body so the Notes tab has content.

## Action Items

- Ship the e2e smoke specs (owner: tenglin) (due: 2026-05-15)
`;

test.describe("review rail", () => {
  test("loads meeting + note and renders the Actions tab rows", async ({
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
      heron_meeting_summary: {
        kind: "ok",
        data: {
          meeting_id: SESSION_ID,
          generated_at: "2026-04-30T12:35:00Z",
          text: NOTE_BODY,
          action_items: MEETING.action_items,
          llm_provider: "anthropic",
          llm_model: "claude-sonnet-4-6",
        },
      },
      heron_read_note: NOTE_BODY,
      heron_check_backup: null,
    });

    await page.goto(`/review/${SESSION_ID}`);

    // Page-header H1 carries the `title` attribute; the markdown
    // body's `# Smoke review` H1 also satisfies role+name, so use
    // `getByTitle` to disambiguate.
    await expect(page.getByTitle("Smoke review")).toBeVisible({
      timeout: 10_000,
    });

    await page.getByRole("tab", { name: /^actions$/i }).click();

    await expect(page.getByText(/ship the e2e smoke specs/i)).toBeVisible();
    await expect(page.getByRole("button", { name: /tenglin/i })).toBeVisible();
    // Match the due chip via its formatted year fragment — locale
    // flips can reorder month/day/year, but `2026` is invariant.
    await expect(page.getByRole("button", { name: /due.*2026/i })).toBeVisible();

    const calls = await getCalls(page);
    expect(calls.some((c) => c.cmd === "heron_get_meeting")).toBe(true);
    expect(calls.some((c) => c.cmd === "heron_read_note")).toBe(true);
  });
});
