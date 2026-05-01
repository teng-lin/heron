/**
 * `onboarding.spec.ts` — issue #217 smoke flow #1.
 *
 * Drives the §13.3 wizard end-to-end with mock IPC returning `pass`
 * outcomes for every probe, then asserts that `Finish setup` fires
 * `heron_mark_onboarded` — the contract under test is the wizard →
 * backend handshake that flips the first-run flag.
 *
 * Why drive every step rather than skip-to-end? The `daemon` step's
 * `canAdvance` is `pass`-only (skip is hidden) — the only path to
 * `Finish setup` exercises the wire shape of every probe.
 */

import { expect, test } from "@playwright/test";

import { DEFAULT_SETTINGS, getCalls, mockIpc } from "./_fixture";

test.describe("onboarding wizard", () => {
  test("walks all 7 steps and fires heron_mark_onboarded on Finish", async ({
    page,
  }) => {
    const passOutcome = { status: "pass", details: "ok" };
    await mockIpc(page, {
      heron_read_settings: { ...DEFAULT_SETTINGS, onboarded: false },
      heron_test_microphone: passOutcome,
      heron_test_audio_tap: passOutcome,
      heron_test_accessibility: passOutcome,
      heron_test_calendar: passOutcome,
      heron_download_model: "WhisperKit model ready",
      heron_run_runtime_checks: [
        {
          name: "onnx_runtime",
          severity: "pass",
          summary: "ONNX runtime available",
          detail: "loaded",
        },
      ],
      heron_test_daemon: passOutcome,
      heron_mark_onboarded: null,
    });
    await page.goto("/");

    await expect(page).toHaveURL(/\/onboarding$/, { timeout: 10_000 });

    const nextBtn = page.getByRole("button", { name: /^next/i });
    // Step 1 → 4: Test → Next.
    for (let i = 0; i < 4; i++) {
      await page
        .getByRole("button", { name: /^(Test|Re-run test)$/i })
        .click();
      await expect(nextBtn).toBeEnabled();
      await nextBtn.click();
    }

    // Step 5 (model_download) — Download instead of Test.
    await page
      .getByRole("button", { name: /^(Download|Re-download)$/i })
      .click();
    await expect(nextBtn).toBeEnabled();
    await nextBtn.click();

    // Step 6 (runtime_checks) — Test + Next.
    await page.getByRole("button", { name: /^(Test|Re-run test)$/i }).click();
    await expect(nextBtn).toBeEnabled();
    await nextBtn.click();

    // Step 7 (daemon) — pass-only gate; Skip is hidden.
    await page.getByRole("button", { name: /^(Test|Re-run test)$/i }).click();
    const finish = page.getByRole("button", { name: /finish setup/i });
    await expect(finish).toBeEnabled();

    await finish.click();

    // Poll the call log rather than racing `toHaveURL` — the post-
    // Finish nav depends on a downstream `resolvePostOnboardingDestination`
    // round-trip whose timing is out of scope here.
    await expect
      .poll(
        async () =>
          (await getCalls(page)).some((c) => c.cmd === "heron_mark_onboarded"),
        { timeout: 5_000 },
      )
      .toBe(true);
  });
});
