/**
 * `recording.spec.ts` — issue #217 smoke flow #2.
 *
 * Drives the recording FSM (start → pause → resume → stop) on the
 * `/recording` page. Each transition asserts the matching IPC fired
 * with the expected `meetingId` arg.
 *
 * We seed an `activeMeeting` via `heron_list_meetings` so the page
 * mounts in the live state — driving the Home Start button would
 * pull in the consent gate, which is the Home spec's domain.
 *
 * Pause/Resume button names match `/^(pause|resume)(?:…|ing…)?$/i`
 * to tolerate the "Pausing…" / "Resuming…" in-flight labels.
 */

import { expect, test, type Page } from "@playwright/test";

import { drainCalls, getCalls, mockIpc } from "./_fixture";

const ACTIVE_MEETING_ID = "mtg_01jeghxxx-7000-0000-0000-000000000001";

const ACTIVE_MEETING = {
  id: ACTIVE_MEETING_ID,
  status: "recording",
  platform: "zoom",
  title: "E2E smoke recording",
  calendar_event_id: null,
  started_at: "2026-04-30T12:00:00Z",
  ended_at: null,
  duration_secs: null,
  participants: [],
  transcript_status: "partial",
  summary_status: "pending",
  tags: [],
};

/**
 * Wait for `cmd` to land in the call log, then drain and return its
 * args. Encapsulates the poll-then-drain pattern the FSM transitions
 * each repeat.
 */
async function expectIpcCall(page: Page, cmd: string): Promise<unknown> {
  await expect
    .poll(
      async () => (await getCalls(page)).find((c) => c.cmd === cmd),
      { timeout: 3_000 },
    )
    .toBeTruthy();
  const calls = await drainCalls(page);
  const call = calls.find((c) => c.cmd === cmd);
  expect(call).toBeDefined();
  return call!.args;
}

test.describe("recording FSM", () => {
  test("start → pause → resume → stop hits the daemon proxy each time", async ({
    page,
  }) => {
    const ack = { kind: "ok", data: { meeting_id: ACTIVE_MEETING_ID } };
    await mockIpc(page, {
      heron_list_meetings: {
        kind: "ok",
        data: { items: [ACTIVE_MEETING], next_cursor: null },
      },
      heron_get_meeting: { kind: "ok", data: ACTIVE_MEETING },
      heron_meeting_transcript: {
        kind: "ok",
        data: {
          meeting_id: ACTIVE_MEETING_ID,
          status: "partial",
          language: "en",
          segments: [],
        },
      },
      heron_pause_meeting: ack,
      heron_resume_meeting: ack,
      heron_end_meeting: ack,
    });

    await page.goto("/recording");

    // Eyebrow text confirms `activeMeeting` resolved — the page
    // mounts the empty-state placeholder while the meetings load is
    // in flight.
    await expect(page.getByText(/recording · clio/i)).toBeVisible({
      timeout: 10_000,
    });

    const pauseBtn = () =>
      page.getByRole("button", { name: /^pause(?:…|ing…)?$/i });
    const resumeBtn = () =>
      page.getByRole("button", { name: /^resume(?:…|ing…)?$/i });

    await expect(pauseBtn()).toBeEnabled();
    await drainCalls(page);

    await pauseBtn().click();
    expect((await expectIpcCall(page, "heron_pause_meeting")) as {
      meetingId: string;
    }).toEqual({ meetingId: ACTIVE_MEETING_ID });
    await expect(resumeBtn()).toBeVisible();

    await resumeBtn().click();
    expect((await expectIpcCall(page, "heron_resume_meeting")) as {
      meetingId: string;
    }).toEqual({ meetingId: ACTIVE_MEETING_ID });
    await expect(pauseBtn()).toBeVisible();

    await page.getByRole("button", { name: /stop & save/i }).click();
    expect((await expectIpcCall(page, "heron_end_meeting")) as {
      meetingId: string;
    }).toEqual({ meetingId: ACTIVE_MEETING_ID });

    // Stop redirects to /review/<id>; pinning the redirect catches a
    // regression that drops the encodeURIComponent.
    await expect(page).toHaveURL(
      new RegExp(`/review/${ACTIVE_MEETING_ID}$`),
      { timeout: 5_000 },
    );
  });
});
