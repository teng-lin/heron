/**
 * `app-launch.spec.ts` — issue #191 / #220 nightly tauri-driver smoke.
 *
 * Drives the packaged `heron-desktop` binary via
 * `@crabnebula/tauri-driver`'s WebDriver bridge. The test verifies the
 * full Rust + JS bundle launches and the first render lands without
 * console errors. The smoke surface is intentionally narrow:
 *
 *  - The packaged binary boots and the WebView reaches `/home`.
 *  - The renderer logs zero `console.error` / `window.onerror` events.
 *
 * This is the only spec in the `tauri` project today. The per-PR
 * `smoke` lane (against the dev server with mocked IPC) carries the
 * day-to-day cost — the nightly lane catches launch-blocking
 * regressions the smoke lane can't see (notarisation gate, broken
 * Rust binary, CSP misconfig, asset-protocol failure).
 *
 * Driver wiring (per the canonical Tauri WebDriver docs at
 * https://v2.tauri.app/develop/tests/webdriver/example/selenium/):
 *
 *  - `tauri-driver` is spawned as a subprocess. It proxies WebDriver
 *    calls between Selenium-style clients and the OS-native WebView
 *    (WebKitWebDriver on macOS / Linux, MSEdgeDriver on Windows). The
 *    package's bin lives at `node_modules/.bin/tauri-driver`.
 *  - The driver listens on `127.0.0.1:4444`. We point a
 *    `selenium-webdriver` `Builder` at that endpoint with the magic
 *    `tauri:options.application` capability set to the packaged
 *    binary path and `browserName: "wry"` (the Tauri WebView name).
 *  - The packaged binary path comes from `HERON_DESKTOP_BINARY`
 *    (CI sets it after `bun run tauri build --no-bundle`); local
 *    runs need to set it explicitly.
 *
 * Why `selenium-webdriver` (not `webdriverio`):
 *
 *  - Issue #220 lists both as acceptable. The Tauri docs' canonical
 *    example uses `selenium-webdriver`, so this picks the lowest-
 *    deviation path from documented upstream wiring.
 *  - `webdriverio` ships its own test runner and expectation library
 *    that overlap Playwright's. We're already inside Playwright's
 *    runner; pulling in a second one risks contention. The `selenium-
 *    webdriver` package is a thin client only — clean fit.
 *
 * Local-dev invocation: see `e2e/tauri/README.md`.
 */

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";

import { expect, test } from "@playwright/test";
import { Builder, Capabilities, type WebDriver } from "selenium-webdriver";

const BINARY_ENV = "HERON_DESKTOP_BINARY";
const TAURI_DRIVER_URL = "http://127.0.0.1:4444";
const HOME_ROUTE = /\/home(?:$|[/?#])/;

// Tight poll on the driver's bind to :4444. selenium's Builder retries
// only briefly on connection refusal, and a fetch poll gives a clearer
// error message than a thrown selenium timeout.
const DRIVER_BIND_TIMEOUT_MS = 5_000;
const DRIVER_BIND_POLL_MS = 100;

test.describe("tauri app launch", () => {
  test.skip(
    !process.env[BINARY_ENV],
    `set ${BINARY_ENV} to the packaged heron-desktop binary path to run this spec`,
  );

  let driverProcess: ChildProcessWithoutNullStreams | undefined;
  let driver: WebDriver | undefined;

  test.beforeAll(async () => {
    const binary = process.env[BINARY_ENV];
    expect(binary, `${BINARY_ENV} must be set`).toBeTruthy();
    expect(
      existsSync(binary!),
      `${BINARY_ENV} points at ${binary}, which does not exist on disk`,
    ).toBe(true);

    // Locate the tauri-driver bin from the local node_modules.
    // `bunx tauri-driver` would also work but adds a layer of process
    // wrapping that complicates teardown — direct spawn keeps the
    // child PID under our control.
    const driverBin = path.resolve(
      __dirname,
      "..",
      "..",
      "node_modules",
      ".bin",
      "tauri-driver",
    );
    expect(
      existsSync(driverBin),
      `tauri-driver binary missing at ${driverBin} — run \`bun install\``,
    ).toBe(true);

    driverProcess = spawn(driverBin, [], { stdio: "pipe" });

    // Surface stderr in the Playwright report so a CI failure points
    // at the driver, not at our spec. stdout is intentionally
    // dropped — tauri-driver's protocol chatter is noise.
    driverProcess.stderr.on("data", (chunk: Buffer) => {
      process.stderr.write(`[tauri-driver] ${chunk.toString()}`);
    });

    // Any HTTP response (any status) means tauri-driver is bound;
    // ECONNREFUSED throws and keeps us polling.
    const deadline = Date.now() + DRIVER_BIND_TIMEOUT_MS;
    while (Date.now() < deadline) {
      try {
        await fetch(`${TAURI_DRIVER_URL}/status`);
        break;
      } catch {
        await new Promise((r) => setTimeout(r, DRIVER_BIND_POLL_MS));
      }
    }

    const capabilities = new Capabilities();
    capabilities.set("tauri:options", { application: binary });
    // `wry` is the Tauri WebView's `browserName` — tauri-driver
    // routes to the OS-native WebDriver based on this. See
    // https://v2.tauri.app/develop/tests/webdriver/example/selenium/.
    capabilities.setBrowserName("wry");

    driver = await new Builder()
      .withCapabilities(capabilities)
      .usingServer(TAURI_DRIVER_URL)
      .build();
  });

  test.afterAll(async () => {
    // Order matters: quit the WebDriver session first so
    // tauri-driver flushes the WebView teardown cleanly, THEN kill
    // the driver process. Reversing this leaves zombie child
    // processes (the OS-native WebDriver server tauri-driver spawns).
    if (driver) {
      try {
        await driver.quit();
      } catch (err) {
        process.stderr.write(`[tauri] driver.quit failed: ${String(err)}\n`);
      }
    }
    if (driverProcess && !driverProcess.killed) {
      driverProcess.kill();
    }
  });

  test("packaged app boots and renders /home with no console errors", async () => {
    expect(driver, "WebDriver session must be established").toBeDefined();
    const session = driver!;

    // Install console + error capture as soon as the WebView document
    // is reachable. Selenium can't run init-scripts pre-navigation
    // the way Playwright's `addInitScript` can, so we accept that
    // boot-time errors logged before this script lands are missed —
    // the smoke lane's `page.on('console')` covers that ground for
    // the renderer; the tauri lane's job is to catch errors that
    // surface AFTER the document is up (IPC handler registration,
    // post-mount React effects, late asset-protocol fetches).
    await session.executeScript(`
      window.__heron_e2e_errors__ = [];
      const origError = console.error.bind(console);
      console.error = (...args) => {
        try {
          window.__heron_e2e_errors__.push(args.map(String).join(" "));
        } catch (_) {}
        origError(...args);
      };
      window.addEventListener("error", (ev) => {
        window.__heron_e2e_errors__.push("pageerror: " + (ev.message || String(ev.error)));
      });
      window.addEventListener("unhandledrejection", (ev) => {
        window.__heron_e2e_errors__.push("unhandledrejection: " + String(ev.reason));
      });
    `);

    // Wait for the React tree to mount before driving navigation —
    // the first-run gate's redirect runs in a `useEffect`, so a fixed
    // sleep would be brittle.
    await session.wait(async () => {
      return (await session.executeScript(
        "return document.readyState === 'complete' && !!document.body.textContent;",
      )) as boolean;
    }, 20_000);

    // Drive directly to `/home` via the in-page router. The packaged
    // binary serves the renderer at a custom asset:// scheme, so a
    // raw `driver.get('/home')` would 404 — pushing through React
    // Router's history API is the equivalent of `page.goto('/home')`
    // in the smoke lane. If the first-run gate then redirects back
    // (e.g. onboarding incomplete), the assertion below catches it
    // as a real failure: the smoke lane's mock pre-seeds
    // `onboarded: true`, but here we exercise whatever the freshly-
    // built binary actually does.
    await session.executeScript(`
      window.history.pushState({}, '', '/home');
      window.dispatchEvent(new PopStateEvent('popstate'));
    `);

    await session.wait(async () => {
      const url = await session.getCurrentUrl();
      return HOME_ROUTE.test(url);
    }, 10_000);

    const url = await session.getCurrentUrl();
    expect(url).toMatch(HOME_ROUTE);

    // Mirror the smoke lane's "zero console errors" gate.
    const errors = (await session.executeScript(
      "return window.__heron_e2e_errors__ || [];",
    )) as string[];
    expect(errors).toEqual([]);
  });
});
