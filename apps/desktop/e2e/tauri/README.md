# `e2e/tauri/` — packaged-app launch smoke

The `tauri` Playwright project drives the **packaged** `heron-desktop`
binary via [`@crabnebula/tauri-driver`][driver]'s WebDriver bridge,
using `selenium-webdriver` as the client. It is the nightly counterpart
to the per-PR `e2e/smoke/` lane (which runs the renderer against the
Vite dev server with mocked IPC).

The lane's job is to catch launch-blocking regressions the smoke lane
cannot see — broken Rust binary, CSP misconfig, asset-protocol failure,
notarisation gate, IPC handler registration drift.

## What runs

A single spec — `app-launch.spec.ts` — that:

1. Spawns `tauri-driver` (binds `127.0.0.1:4444`).
2. Builds a `selenium-webdriver` session with `browserName: "wry"` and
   `tauri:options.application` set to the packaged binary path.
3. Waits for the renderer to mount, drives the in-page router to
   `/home`, and asserts zero `console.error` / `window.onerror` /
   `unhandledrejection` events.
4. Tears down the WebDriver session and kills `tauri-driver`.

## Why `selenium-webdriver` (not `webdriverio`)

- Issue #220 lists both as acceptable. The canonical
  [Tauri WebDriver docs example][selenium-example] uses
  `selenium-webdriver`, so we follow the lowest-deviation path from
  upstream's documented wiring.
- `webdriverio` ships its own test runner and expectation library that
  overlap Playwright's. We are already inside Playwright's runner;
  pulling in a second one risks contention. `selenium-webdriver` is a
  thin client only — clean fit alongside `@playwright/test`.

## Local invocation

The spec is gated on the `HERON_DESKTOP_BINARY` env var. Without it the
test skips cleanly so the smoke lane's `bun run e2e` doesn't drag the
tauri lane along.

```sh
# 1. Build the packaged binary (one-shot; cache reuse on subsequent
#    runs). `--no-bundle` skips the .app/.dmg wrap + codesign step we
#    don't need for a launch smoke.
cd apps/desktop
bun run tauri build --no-bundle

# 2. Run the tauri lane against the freshly-built binary.
HERON_DESKTOP_BINARY="$(pwd)/src-tauri/target/release/heron-desktop" \
  bunx playwright test --project=tauri
```

Platform notes:

- **macOS**: `tauri-driver` proxies to `WebKitWebDriver`, which ships
  with the system. No extra install.
- **Linux**: install `webkit2gtk-driver` (Debian/Ubuntu) or the
  equivalent.
- **Windows**: install Microsoft Edge WebDriver matching the runner's
  Edge version. The Tauri docs cover this in detail.

## CI invocation

The nightly `tauri-driver` job in `.github/workflows/nightly.yml`:

1. Builds the packaged binary on `macos-14`.
2. Sets `HERON_DESKTOP_BINARY` to the artifact path.
3. Runs `bun run e2e:tauri` (`playwright test --project=tauri`).
4. Uploads the Playwright report on failure as a workflow artifact.

The lane only runs on the nightly cron — the build cost is too high
for a per-PR gate, and the smoke lane already covers the renderer
surface.

[driver]: https://www.npmjs.com/package/@crabnebula/tauri-driver
[selenium-example]: https://v2.tauri.app/develop/tests/webdriver/example/selenium/
