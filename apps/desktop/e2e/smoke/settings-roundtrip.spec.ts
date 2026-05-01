/**
 * `settings-roundtrip.spec.ts` — issue #191 smoke flow #6.
 *
 * Tier-1 controls persist:
 *  - Toggle "Recover on launch" on the General tab.
 *  - Wait for the autosave debounce to fire `heron_write_settings`.
 *  - Assert the call payload mirrors the user's edit.
 *
 * The autosave is the contract under test — `Settings.tsx`'s 500 ms
 * debounce + the in-flight save coalescer. Manual Save click is
 * covered by the same path; the autosave covers it transitively, so
 * one test is enough.
 *
 * What we DON'T cover here:
 *  - Backend persistence (`heron_write_settings` is mocked; the
 *    Rust-side round-trip is `apps/desktop/src-tauri/tests/`'s
 *    domain, see PR #208).
 *  - Per-field types (covered by `bun test` unit suites).
 */

import { expect, test } from "@playwright/test";

import { drainCalls, getCalls, mockIpc, DEFAULT_SETTINGS } from "./_fixture";

test.describe("settings round-trip", () => {
  test("toggling Recover on launch fires heron_write_settings with the new value", async ({
    page,
  }) => {
    // Pre-seed `recover_on_launch: false` BEFORE the renderer boots —
    // `addInitScript` runs once per page navigation, so the override
    // has to land in the same call that installs the harness. Doing
    // the override post-`goto` and then `page.reload()` doesn't work
    // because `setIpcRoutes` writes via `page.evaluate`, which is
    // wiped on reload.
    await mockIpc(page, {
      heron_read_settings: { ...DEFAULT_SETTINGS, recover_on_launch: false },
    });
    await page.goto("/settings");

    await expect(page.getByTestId("settings-page")).toBeVisible();

    // Drain any boot-time calls so the assertion below sees only the
    // post-toggle write.
    await drainCalls(page);

    // The toggle is wired through the General tab — it's the default
    // tab so no click needed.
    const toggle = page.getByRole("switch", { name: /recover on launch/i });
    await expect(toggle).toBeVisible();
    await expect(toggle).toHaveAttribute("data-state", "unchecked");

    await toggle.click();
    await expect(toggle).toHaveAttribute("data-state", "checked");

    // The autosave debounce is 500 ms; give Playwright generous
    // headroom (3 s) so a slow CI runner doesn't false-fail. `getCalls`
    // (non-draining) keeps polled retries idempotent — `drainCalls`
    // would lose the entry between polls.
    await expect
      .poll(
        async () => {
          const calls = await getCalls(page);
          return calls.find((c) => c.cmd === "heron_write_settings");
        },
        { timeout: 3_000 },
      )
      .toBeTruthy();

    const calls = await drainCalls(page);
    const writeCall = calls.find((c) => c.cmd === "heron_write_settings");
    expect(writeCall).toBeDefined();
    const args = writeCall!.args as {
      settingsPath: string;
      settings: { recover_on_launch: boolean };
    };
    expect(args.settings.recover_on_launch).toBe(true);
  });
});
