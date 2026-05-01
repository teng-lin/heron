/**
 * Playwright config — issue #191.
 *
 * Two projects, two lanes:
 *
 * - `smoke` (per-PR) — runs the renderer against the Vite dev server.
 *   IPC is mocked at `window.__TAURI_INTERNALS__.invoke` via
 *   `addInitScript` (see `e2e/smoke/_fixture.ts`). No Rust process
 *   involved, no notarization, no driver. CI-cheap and fast.
 *
 * - `tauri` (nightly) — drives the packaged app via
 *   `@crabnebula/tauri-driver`'s WebDriver bridge. Verifies the full
 *   Rust + JS bundle launches and the first render lands without IPC
 *   errors. Skipped on PRs because the build cost is high.
 *
 * The two lanes share zero spec files — `testDir` is per-project. The
 * shared `e2e/fixtures/` dir holds factory helpers both can import.
 *
 * Config notes:
 * - `webServer` only fires for the `smoke` lane (it's a dev-server
 *   harness; the `tauri` lane talks to an already-built binary). The
 *   `command` matches `apps/desktop/package.json::dev` and reuses the
 *   port pinned in `vite.config.ts`.
 * - `forbidOnly: !!process.env.CI` keeps a stray `test.only` from
 *   green-washing CI without blocking interactive `--ui` runs.
 * - `workers: process.env.CI ? 1 : undefined` — Playwright defaults to
 *   parallel-by-file. Single worker on CI keeps the dev server's port
 *   monopoly safe; locally we let Playwright pick.
 * - `timeout: 30_000` per test + a project-level `expect.timeout`
 *   keeps the whole smoke suite under 2 minutes wall-time even when a
 *   single page is slow to mount.
 */

import { defineConfig, devices } from "@playwright/test";

const PORT = 1420;
const BASE_URL = `http://localhost:${PORT}`;

export default defineConfig({
  // Per-project `testDir` keeps smoke + tauri specs in disjoint trees.
  // The top-level `testDir` is a fallback only used when a project
  // doesn't override it.
  testDir: "./e2e",
  // Smoke specs share one Vite dev server; running them in parallel
  // workers would double-bind the pinned 1420 port. `fullyParallel:
  // false` AND `workers: 1` would be redundant — keep `workers: 1`
  // on CI as the authoritative single-worker gate, and let local
  // runs default to Playwright's CPU-based pick (the dev server's
  // `reuseExistingServer: true` makes parallel local runs work).
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: process.env.CI ? [["github"], ["list"]] : "list",
  timeout: 30_000,
  expect: { timeout: 5_000 },

  use: {
    baseURL: BASE_URL,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "off",
  },

  projects: [
    {
      name: "smoke",
      testDir: "./e2e/smoke",
      testMatch: /.*\.spec\.ts$/,
      use: {
        ...devices["Desktop Chrome"],
        baseURL: BASE_URL,
      },
    },
    {
      name: "tauri",
      testDir: "./e2e/tauri",
      testMatch: /.*\.spec\.ts$/,
      // The tauri lane talks to tauri-driver (WebDriver), not a Vite
      // dev server, so `baseURL` doesn't apply. The driver wiring
      // lives in the spec's setup helper.
    },
  ],

  // Boots `vite dev` for the smoke lane only. The tauri lane uses a
  // pre-built app binary launched by `tauri-driver`, not a dev server.
  webServer: process.env.PLAYWRIGHT_SKIP_WEBSERVER
    ? undefined
    : {
        command: "bun run dev",
        url: BASE_URL,
        reuseExistingServer: !process.env.CI,
        timeout: 60_000,
        stdout: "pipe",
        stderr: "pipe",
      },
});
