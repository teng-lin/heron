/**
 * Playwright fixture — installs a `window.__TAURI_INTERNALS__.invoke`
 * stub before the renderer boots.
 *
 * Why this shape (not a Vite alias):
 *
 * The Bun unit-test pattern (`mock.module("../lib/invoke", ...)` —
 * see `apps/desktop/src/store/calendar.test.ts`) is import-graph
 * surgery that doesn't translate to a real browser. A Vite alias would
 * have to normalize across `./`, `../`, `../../` import depths and
 * still keep production builds out of the alias path; that's a
 * footgun the issue spec explicitly flagged.
 *
 * Instead we lean on the canonical Tauri pattern: the official
 * `@tauri-apps/api/core` module reads its IPC handle from
 * `window.__TAURI_INTERNALS__.invoke`. Playwright's `addInitScript`
 * runs BEFORE any page script, so seeding that global there means the
 * very first `tauriInvoke(...)` the bundle makes is served by our mock.
 *
 * The mock body is inlined into `addInitScript` because Playwright
 * serializes the function to a string before injecting it — the page
 * context cannot reach any outer-scope helper. The duplication
 * between the TS-side `DEFAULT_SETTINGS` and the case statement
 * inside `addInitScript` is structural, not avoidable.
 */

import type { Page } from "@playwright/test";

/**
 * Default `Settings` payload surfaced by `heron_read_settings`. Mirrors
 * the Rust `Settings::default` shape closely enough for the renderer
 * to mount without crashes; specs that exercise specific fields
 * override via `mockIpc(page, { heron_read_settings: ... })`.
 *
 * `onboarded: true` skips the wizard and lands the smoke harness
 * directly on the post-onboarding destination. Keep this default
 * narrow — adding fields here couples every spec to schema drift; if
 * a Settings test needs a richer object, build it in-spec.
 */
export const DEFAULT_SETTINGS = {
  stt_backend: "whisperkit",
  llm_backend: "anthropic",
  auto_summarize: true,
  vault_root: "/tmp/heron-e2e-vault",
  record_hotkey: "CmdOrCtrl+Shift+R",
  remind_interval_secs: 60,
  recover_on_launch: true,
  min_free_disk_mib: 256,
  session_logging: true,
  crash_telemetry: false,
  audio_retention_days: null,
  onboarded: true,
  target_bundle_ids: ["us.zoom.xos"],
  active_mode: "clio",
  hotwords: [],
  persona: { name: "", role: "", working_on: "" },
  file_naming_pattern: "id",
  summary_retention_days: null,
  strip_names_before_summarization: false,
  show_tray_indicator: true,
  auto_detect_meeting_app: false,
  openai_model: "gpt-4o-mini",
  shortcuts: {},
};

/**
 * Install the IPC mock + Tauri event-plugin shim. Must be called
 * BEFORE `page.goto(...)`. `routes` overrides the per-command
 * defaults — pass it on the initial call rather than via a follow-up
 * helper, because `addInitScript` is the only hook that runs before
 * the bundle's first `tauriInvoke`.
 *
 * The handler is intentionally lenient: unknown commands resolve to
 * `null` rather than rejecting, because the smoke tests should not
 * fail on a sibling page's IPC call that the spec doesn't care
 * about. Specs that DO care should override that command via the
 * `routes` arg and assert.
 */
export async function mockIpc(
  page: Page,
  routes: Record<string, unknown> = {},
): Promise<void> {
  const seedJson = JSON.stringify({
    settings: DEFAULT_SETTINGS,
    extraRoutes: routes,
  });

  await page.addInitScript((seed: string) => {
    const parsed = JSON.parse(seed) as {
      settings: Record<string, unknown>;
      extraRoutes: Record<string, unknown>;
    };

    // Mirror @tauri-apps/api/mocks::mockInternals — initialise the
    // globals the bundle reads. Set BOTH internals globals so the
    // event plugin shim below is reachable.
    const w = window as unknown as {
      __TAURI_INTERNALS__?: Record<string, unknown>;
      __TAURI_EVENT_PLUGIN_INTERNALS__?: Record<string, unknown>;
      __heron_e2e_routes__?: Record<string, unknown>;
      __heron_e2e_calls__?: Array<{ cmd: string; args: unknown }>;
    };
    w.__TAURI_INTERNALS__ = w.__TAURI_INTERNALS__ ?? {};
    w.__TAURI_EVENT_PLUGIN_INTERNALS__ = w.__TAURI_EVENT_PLUGIN_INTERNALS__ ?? {};
    w.__heron_e2e_routes__ = { ...parsed.extraRoutes };
    w.__heron_e2e_calls__ = [];

    const eventListeners = new Map<string, Set<number>>();
    const callbacks = new Map<number, (data: unknown) => void>();

    function transformCallback(
      cb: ((data: unknown) => void) | undefined,
      once: boolean,
    ): number {
      const id = window.crypto.getRandomValues(new Uint32Array(1))[0];
      callbacks.set(id, (data) => {
        if (once) callbacks.delete(id);
        if (cb) cb(data);
      });
      return id;
    }

    function defaultRoute(
      cmd: string,
      args: Record<string, unknown> | undefined,
    ): unknown {
      // Event plugin shim — `listen`/`unlisten`/`emit` are routed
      // through `plugin:event|*` IPC calls. We accept them quietly so
      // SSE bridge calls + sonner toasts don't error.
      if (cmd.startsWith("plugin:event|")) {
        if (cmd === "plugin:event|listen") {
          const a = args as { event: string; handler: number } | undefined;
          if (a) {
            const set = eventListeners.get(a.event) ?? new Set();
            set.add(a.handler);
            eventListeners.set(a.event, set);
            return a.handler;
          }
          return 0;
        }
        return null;
      }
      switch (cmd) {
        case "heron_default_settings_path":
          return "/tmp/heron-e2e-settings.json";
        case "heron_default_cache_root":
          return "/tmp/heron-e2e-cache";
        case "heron_read_settings":
          return parsed.settings;
        case "heron_write_settings":
          return null;
        case "heron_status":
          return {
            version: "0.1.0-e2e",
            fsm_state: "idle",
            audio_available: true,
            ax_backend: "mock",
          };
        case "heron_list_meetings":
        case "heron_list_calendar_upcoming":
          return { kind: "ok", data: { items: [], total: 0 } };
        case "heron_scan_unfinalized":
          return [];
        case "heron_check_disk_for_recording":
          return { kind: "ok", free_mib: 100_000 };
        case "heron_last_note_session_id":
          return null;
        case "heron_take_pending_shortcut_conflicts":
          return [];
        case "heron_subscribe_events":
        case "heron_unsubscribe_events":
          return null;
        case "heron_daemon_status":
          return { running: true, version: "0.1.0-e2e", error: null };
        case "heron_keychain_list":
          return [];
        default:
          return null;
      }
    }

    // eslint-disable-next-line @typescript-eslint/require-await
    async function invoke(
      cmd: string,
      args: Record<string, unknown> | undefined,
      _options?: unknown,
    ): Promise<unknown> {
      w.__heron_e2e_calls__?.push({ cmd, args });
      const overrides = w.__heron_e2e_routes__ ?? {};
      if (cmd in overrides) {
        const value = (overrides as Record<string, unknown>)[cmd];
        if (typeof value === "function") {
          return (value as (a: typeof args) => unknown)(args);
        }
        return value;
      }
      return defaultRoute(cmd, args);
    }

    w.__TAURI_INTERNALS__.invoke = invoke;
    w.__TAURI_INTERNALS__.transformCallback = transformCallback;
    w.__TAURI_INTERNALS__.unregisterCallback = (id: number) => {
      callbacks.delete(id);
    };
    w.__TAURI_INTERNALS__.runCallback = (id: number, data: unknown) => {
      const cb = callbacks.get(id);
      if (cb) cb(data);
    };
    w.__TAURI_INTERNALS__.callbacks = callbacks;
    w.__TAURI_INTERNALS__.metadata = {
      currentWindow: { label: "main" },
      currentWebview: { windowLabel: "main", label: "main" },
    };
    w.__TAURI_INTERNALS__.convertFileSrc = (
      filePath: string,
      protocol = "asset",
    ) => `${protocol}://localhost/${encodeURIComponent(filePath)}`;
    w.__TAURI_EVENT_PLUGIN_INTERNALS__.unregisterListener = () => {};
  }, seedJson);
}

/**
 * Drain the call log captured by the mock. Returns one entry per
 * `invoke()` the renderer made, in order. Resets the log on read so
 * subsequent assertions don't see prior-test leakage.
 */
export async function drainCalls(
  page: Page,
): Promise<Array<{ cmd: string; args: unknown }>> {
  return page.evaluate(() => {
    const w = window as unknown as {
      __heron_e2e_calls__?: Array<{ cmd: string; args: unknown }>;
    };
    const log = w.__heron_e2e_calls__ ?? [];
    w.__heron_e2e_calls__ = [];
    return log;
  });
}
