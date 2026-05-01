/**
 * `timezone.spec.ts` — issue #217 smoke flow #4.
 *
 * Pins the calendar-date stability of `formatActionItemDue` across
 * Playwright's per-context `timezoneId`. The function (in
 * `pages/review/utils/format.ts`) deliberately pins each YYYY-MM-DD
 * date's parts manually rather than the UTC-treating
 * `new Date(iso)`, so the rendered chip stays on the date the LLM
 * emitted regardless of the user's TZ. This spec is the regression
 * net for that contract.
 *
 * We hit three offsets: LA (UTC-7/8 — the negative offset the naive
 * implementation drifts on), Tokyo (UTC+9), and UTC (sanity zero).
 */

import {
  expect,
  test,
  type Browser,
  type BrowserContext,
} from "@playwright/test";

import { mockIpc } from "./_fixture";

const SESSION_ID = "mtg_01jegtzz-7000-0000-0000-000000000001";

const ACTION_ITEM_ID = "22222222-2222-7222-8222-222222222222";

const DUE_DATE = "2026-05-15";

const MEETING = {
  id: SESSION_ID,
  status: "done",
  platform: "zoom",
  title: "Timezone smoke",
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
      text: "Pin the calendar date",
      owner: null,
      due: DUE_DATE,
      done: false,
    },
  ],
};

const NOTE_BODY = `# Timezone smoke

A short body.

## Action Items

- Pin the calendar date (due: 2026-05-15)
`;

const TIMEZONES = ["America/Los_Angeles", "Asia/Tokyo", "UTC"] as const;

async function newPageInTz(browser: Browser, timezoneId: string) {
  // Re-create a context per timezone — Playwright's `timezoneId` is a
  // context-level option, not a per-page override.
  const ctx: BrowserContext = await browser.newContext({
    timezoneId,
    locale: "en-US",
  });
  const page = await ctx.newPage();
  return { ctx, page };
}

test.describe("action-item due date is timezone-stable", () => {
  for (const timezoneId of TIMEZONES) {
    test(`renders ${DUE_DATE} consistently in ${timezoneId}`, async ({
      browser,
    }) => {
      const { ctx, page } = await newPageInTz(browser, timezoneId);
      try {
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
        });

        await page.goto(`/review/${SESSION_ID}`);

        // Page-header H1 has the `title` attribute; the markdown-body
        // H1 also satisfies role+name, so use `getByTitle`.
        await expect(page.getByTitle("Timezone smoke")).toBeVisible({
          timeout: 10_000,
        });

        await page.getByRole("tab", { name: /^actions$/i }).click();

        const chip = page.getByRole("button", { name: /due.*2026/i });
        await expect(chip).toBeVisible();
        const chipText = (await chip.textContent()) ?? "";
        expect(chipText).toMatch(/2026/);
        expect(chipText).toMatch(/15/);
        // Negative regression: a naive `new Date("2026-05-15")` would
        // drift to May 14 (LA) or May 16 (Tokyo).
        expect(chipText).not.toMatch(/\b14\b/);
        expect(chipText).not.toMatch(/\b16\b/);
      } finally {
        await ctx.close();
      }
    });
  }
});
