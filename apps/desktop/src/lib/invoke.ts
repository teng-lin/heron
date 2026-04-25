/**
 * Typed wrapper around `@tauri-apps/api/core::invoke`.
 *
 * Mirrors the `#[tauri::command]` surface declared in
 * `apps/desktop/src-tauri/src/lib.rs`. The Rust side derives
 * `Serialize` with `#[serde(rename_all = "snake_case")]` (see
 * `heron_types::RecordingState`, `AssetSource`, `TestOutcome`), so the
 * TS types here use snake_case discriminants where the Rust enum tags
 * surface to the wire.
 *
 * Each command has a paired arg + return type; the discriminated union
 * below makes `invoke("heron_status")` return `HeronStatus` and only
 * accept the empty arg shape, while `invoke("heron_test_audio_tap",
 * { targetBundleId: ... })` rejects calls that omit the argument.
 *
 * PR-α (phase 62) ships only the type plumbing — no caller in the
 * React tree exercises every command yet. The downstream PRs (β, γ,
 * δ) drop in real call sites against this same surface.
 */

import { invoke as tauriInvoke } from "@tauri-apps/api/core";

// ---- Domain types --------------------------------------------------

/**
 * Recording-flow FSM state. Matches the wire format of
 * `heron_types::RecordingState`'s `#[serde(rename_all = "snake_case")]`.
 */
export type RecordingState =
  | "idle"
  | "armed"
  | "armed_cooldown"
  | "recording"
  | "transcribing"
  | "summarizing";

export interface HeronStatus {
  version: string;
  fsm_state: RecordingState;
  audio_available: boolean;
  ax_backend: string;
}

/**
 * Discriminated union mirroring `AssetSource` (tag = "kind").
 */
export type AssetSource =
  | { kind: "m4a"; path: string }
  | { kind: "salvage_from_cache"; mic_raw: string; tap_raw: string };

export interface SessionLogError {
  at: string | null;
  kind: string;
  message: string;
}

export interface DiagnosticsView {
  session_id: string | null;
  ax_hit_rate: number | null;
  dropped_frames: number | null;
  stt_wall_time_secs: number | null;
  llm_cost_usd: number | null;
  error_count: number;
  errors: SessionLogError[];
}

export interface Settings {
  stt_backend: string;
  llm_backend: string;
  auto_summarize: boolean;
  vault_root: string;
  record_hotkey: string;
  remind_interval_secs: number;
  recover_on_launch: boolean;
  min_free_disk_mib: number;
  session_logging: boolean;
  crash_telemetry: boolean;
}

/**
 * Discriminated union mirroring `TestOutcome` (tag = "status").
 */
export type TestOutcome =
  | { status: "pass"; details: string }
  | { status: "fail"; details: string }
  | { status: "needs_permission"; details: string }
  | { status: "skipped"; details: string };

/**
 * Wire-format labels for the macOS-Keychain accounts heron knows
 * about (PR-θ / phase 70). Mirrors `KeychainAccount::as_str` in
 * `apps/desktop/src-tauri/src/keychain.rs`.
 *
 * The Rust shim rejects any unknown label with an error string, so
 * narrowing the type here is belt-and-suspenders rather than a
 * security boundary — but it does keep the call sites honest at
 * compile time.
 */
export type KeychainAccount = "anthropic_api_key" | "openai_api_key";

// ---- Command surface ----------------------------------------------

/**
 * Per-command arg + return mapping.
 *
 * Commands that take no arguments map to `Record<string, never>` so
 * callers can pass `undefined` (or omit the second `invoke` argument).
 *
 * Tauri v2 renames *top-level* command argument keys from camelCase
 * (JS) to snake_case (Rust), so `args` keys here are camelCase
 * (`sessionId`) and pair with snake_case Rust parameters
 * (`session_id`). The rename does **not** recurse into nested
 * payloads: the `settings: Settings` body of `heron_write_settings`
 * is forwarded to serde verbatim, which is why `Settings` (and the
 * other nested types — `AssetSource`, `TestOutcome`,
 * `DiagnosticsView`) keep snake_case keys to match the Rust struct's
 * default field naming. If a future Rust change adds
 * `#[serde(rename_all = "...")]` to one of those types, the matching
 * TS field names here must move in lockstep — TS will not notice the
 * wire-shape drift.
 *
 * Return values come back exactly as the Rust side serialized them
 * (snake_case fields like `fsm_state` and `session_id`).
 */
export interface HeronCommands {
  heron_status: {
    args: Record<string, never>;
    returns: HeronStatus;
  };
  heron_resolve_recording: {
    args: {
      sessionId: string;
      m4aCandidate: string;
      cacheRoot: string;
    };
    returns: AssetSource;
  };
  heron_diagnostics: {
    args: { sessionLogPath: string };
    returns: DiagnosticsView;
  };
  heron_read_settings: {
    args: { settingsPath: string };
    returns: Settings;
  };
  heron_write_settings: {
    args: { settingsPath: string; settings: Settings };
    returns: void;
  };
  heron_default_settings_path: {
    args: Record<string, never>;
    returns: string;
  };
  heron_read_note: {
    args: { vaultPath: string; sessionId: string };
    returns: string;
  };
  heron_write_note_atomic: {
    args: { vaultPath: string; sessionId: string; contents: string };
    returns: void;
  };
  heron_list_sessions: {
    args: { vaultPath: string };
    returns: string[];
  };
  heron_test_microphone: {
    args: Record<string, never>;
    returns: TestOutcome;
  };
  heron_test_audio_tap: {
    args: { targetBundleId: string };
    returns: TestOutcome;
  };
  heron_test_accessibility: {
    args: Record<string, never>;
    returns: TestOutcome;
  };
  heron_test_calendar: {
    args: Record<string, never>;
    returns: TestOutcome;
  };
  heron_test_model_download: {
    args: Record<string, never>;
    returns: TestOutcome;
  };
  /**
   * Phase 64 (PR-β): focus the main window and emit `nav:<target>` so
   * the React tree can navigate. Recognised targets: `"settings"`,
   * `"recording"`. Unknown targets reject the promise.
   */
  heron_open_window: {
    args: { target: string };
    returns: void;
  };
  /**
   * Phase 70 (PR-θ): store an API-key secret in the macOS login
   * Keychain. The Rust side never logs `secret`, never echoes it
   * back, and never returns it across the IPC bridge. Replaces an
   * existing entry on the same `account` slot. Errors stringly:
   * unknown account labels, backend failures, or `Unsupported` on
   * non-macOS.
   *
   * `secret` should come straight from a password input the user
   * just typed; do not stash it in component state, route state, or
   * localStorage on the JS side.
   */
  heron_keychain_set: {
    args: { account: KeychainAccount; secret: string };
    returns: void;
  };
  /**
   * Phase 70 (PR-θ): existence probe — returns `true` iff the named
   * account currently has a stored entry. **Does NOT return the
   * secret value.** This is the only command the UI uses to render
   * "set / not set" status without ever pulling the cleartext into
   * the renderer.
   */
  heron_keychain_has: {
    args: { account: KeychainAccount };
    returns: boolean;
  };
  /**
   * Phase 70 (PR-θ): delete the entry for the named account.
   * Idempotent — deleting a missing entry resolves to `void`.
   */
  heron_keychain_delete: {
    args: { account: KeychainAccount };
    returns: void;
  };
  /**
   * Phase 70 (PR-θ): enumerate the wire-format labels of accounts
   * that currently have entries. Returns a subset of the
   * `KeychainAccount` union; the Rust side is the single source of
   * truth for what's known.
   */
  heron_keychain_list: {
    args: Record<string, never>;
    returns: KeychainAccount[];
  };
}

export type HeronCommand = keyof HeronCommands;

// ---- The wrapper --------------------------------------------------

/**
 * Type-safe `invoke`. Resolves to the per-command return type and
 * rejects calls whose `args` shape doesn't match the command.
 *
 * The body is a thin delegate; all the value lives in the type
 * signature.
 */
export async function invoke<C extends HeronCommand>(
  cmd: C,
  ...rest: HeronCommands[C]["args"] extends Record<string, never>
    ? [args?: undefined]
    : [args: HeronCommands[C]["args"]]
): Promise<HeronCommands[C]["returns"]> {
  const [args] = rest;
  // Tauri's `invoke` signature accepts `InvokeArgs | undefined`; the
  // cast keeps the public surface tight while delegating the actual
  // serialization to the Tauri runtime.
  return tauriInvoke<HeronCommands[C]["returns"]>(
    cmd,
    args as Record<string, unknown> | undefined,
  );
}
