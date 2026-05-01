/**
 * `salvage.spec.ts` — issue #217 smoke flow #6.
 *
 * Drives the crash-recovery path on `/salvage`: the page scans for
 * unfinalized sessions via `heron_scan_unfinalized`, the user clicks
 * Recover on a row, confirms the dialog, and the spec asserts
 * `heron_recover_session` fires with the row's session id.
 *
 * The Recover handler also pulls `vaultPath` out of the user's
 * settings (`heron_default_settings_path` → `heron_read_settings`),
 * so we mock those too. The default settings from `_fixture.ts`'s
 * `DEFAULT_SETTINGS` already supplies `vault_root`; we override
 * only the scan + recover commands here.
 *
 * "Abort mid-record" is the *cause* the issue spec names — the
 * unfinalized session is the artefact a SIGKILL'd recording leaves
 * behind. The salvage list is what the user sees on the next
 * launch; this spec exercises the recover path the user takes from
 * there. We don't try to reproduce the abort itself in a smoke
 * test; that would need a real audio pipeline.
 */

import { expect, test } from "@playwright/test";

import { DEFAULT_SETTINGS, drainCalls, getCalls, mockIpc } from "./_fixture";

const STRANDED_SESSION_ID = "mtg_01jegslv-7000-0000-0000-000000000001";

const STRANDED_SESSION = {
  session_id: STRANDED_SESSION_ID,
  started_at: "2026-04-30T11:00:00Z",
  audio_bytes: 1_234_567,
  has_partial_transcript: true,
};

test.describe("salvage", () => {
  test("Recover fires heron_recover_session with the row's session id", async ({
    page,
  }) => {
    await mockIpc(page, {
      heron_scan_unfinalized: [STRANDED_SESSION],
      // The Rust handler returns the recovered note's path — the
      // page doesn't read the value (it just navigates to /review/<id>),
      // but the promise needs to resolve, not reject, for the success
      // path to fire.
      heron_recover_session: `${DEFAULT_SETTINGS.vault_root}/recovered.md`,
    });

    await page.goto("/salvage");

    // The session id renders inside a `<p class="font-mono">`. Match
    // on the literal text.
    await expect(page.getByText(STRANDED_SESSION_ID)).toBeVisible({
      timeout: 10_000,
    });

    // Drain boot calls (notably the `heron_scan_unfinalized` initial
    // load) so the post-click assertion sees only the recover IPC.
    await drainCalls(page);

    // Click Recover on the row. The button is a Radix `<Button>`
    // (`<button>`), name match by accessible text.
    await page.getByRole("button", { name: /^recover$/i }).click();

    // Confirmation dialog opens — Radix Dialog renders a `role=dialog`.
    // The confirm button inside it is also "Recover". Use `getByRole`
    // scoped to the dialog so we don't double-click the row button.
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    await dialog.getByRole("button", { name: /^recover$/i }).click();

    await expect
      .poll(
        async () => {
          const calls = await getCalls(page);
          return calls.find((c) => c.cmd === "heron_recover_session");
        },
        { timeout: 5_000 },
      )
      .toBeTruthy();

    const calls = await drainCalls(page);
    const call = calls.find((c) => c.cmd === "heron_recover_session");
    expect(call).toBeDefined();
    const args = call!.args as { sessionId: string; vaultPath: string };
    expect(args.sessionId).toBe(STRANDED_SESSION_ID);
    // `vaultPath` is sourced from `_fixture.ts`'s DEFAULT_SETTINGS;
    // pin the same constant so a regression that hardcodes a stale
    // path or sources from a different store would surface here.
    expect(args.vaultPath).toBe(DEFAULT_SETTINGS.vault_root);

    // Successful recovery navigates to /review/<id>. Confirm the
    // post-recover redirect landed.
    await expect(page).toHaveURL(
      new RegExp(`/review/${STRANDED_SESSION_ID}$`),
      { timeout: 5_000 },
    );
  });
});
