/**
 * `app-launch.spec.ts` — issue #191 smoke flow #1.
 *
 * Acceptance:
 *  - The renderer boots against the Vite dev server with mocked IPC.
 *  - First-run gate routes to `/home` (because `onboarded: true`).
 *  - The Settings route mounts when the user navigates there.
 *  - No console errors.
 *
 * The "no console errors" check is the most fragile gate — the
 * matrix of upstream warnings (React 19 dev-mode strict-mode,
 * Tailwind v4 CSS layer ordering, sonner registration) means we
 * filter on `error` severity only and ignore well-known noise.
 */

import { expect, test } from "@playwright/test";

import { mockIpc } from "./_fixture";

test.describe("app launch", () => {
  test("boots, lands on /home, Settings route mounts, no console errors", async ({
    page,
  }) => {
    const errors: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") {
        errors.push(msg.text());
      }
    });
    page.on("pageerror", (err) => {
      errors.push(`pageerror: ${err.message}`);
    });

    await mockIpc(page);
    await page.goto("/");

    // The first-run gate is async — wait for the post-onboarding
    // redirect (default `onboarded: true` -> /home, vault empty so
    // no /review/<latest> redirect).
    await expect(page).toHaveURL(/\/home$/, { timeout: 10_000 });

    // Sidebar's "All meetings" nav lives under `<aside>` chrome.
    // Use a stable role match instead of a brittle CSS path.
    await expect(
      page.getByRole("button", { name: /all meetings/i }),
    ).toBeVisible();

    // Navigate to /settings via the title-bar Settings button so we
    // exercise the actual route boundary, not just a direct goto.
    await page.getByRole("button", { name: /^settings$/i }).click();
    await expect(page).toHaveURL(/\/settings$/);

    await expect(page.getByTestId("settings-page")).toBeVisible();
    await expect(
      page.getByRole("heading", { name: /^settings$/i }),
    ).toBeVisible();

    // The General tab is the default — confirm it rendered.
    await expect(page.getByTestId("settings-tab-general")).toBeVisible();

    // Spec acceptance: zero console errors during boot + first nav.
    expect(errors).toEqual([]);
  });
});
