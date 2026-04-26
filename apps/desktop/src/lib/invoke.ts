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

/**
 * Mirrors the Rust `BackupInfo` struct returned by `heron_check_backup`.
 *
 * `created_at` is the `<id>.md.bak` file's modification time as an
 * RFC-3339 string in the system's local timezone. The Review UI feeds
 * it through `Intl.DateTimeFormat` for the "Backup from <timestamp>"
 * pill.
 */
export interface BackupInfo {
  created_at: string;
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
  /**
   * PR-ζ (phase 68). `null` means "keep all" — the backend's
   * `Option<u32>` deserializes `None` as `null` over the IPC bridge.
   * The Audio tab radio binds to this directly: `null` → "Keep all",
   * `42` → "Purge audio older than 42 days".
   */
  audio_retention_days: number | null;
  /**
   * PR-ι (phase 71). `true` once the user has finished the §13.3
   * five-step onboarding wizard. The Rust struct's container-level
   * `#[serde(default)]` deserializes pre-PR-71 settings.json files
   * with `onboarded = false` so the wizard runs once after upgrade —
   * no migration ceremony needed on the JS side. `App.tsx`'s
   * first-run detection branches on this field.
   */
  onboarded: boolean;
  /**
   * PR-λ (phase 73). Bundle IDs the user wants the Core Audio process
   * tap to record. Defaults to `["us.zoom.xos"]` for backward compat
   * with PR-α; the Settings → Audio "Recorded apps" card lets the
   * user add Microsoft Teams / Google Chrome / other meeting clients.
   */
  target_bundle_ids: string[];
}

/**
 * Discriminated union mirroring the Rust `DiskCheckOutcome`
 * (`tag = "kind"`, lowercase variants). Returned by
 * `heron_check_disk_for_recording`.
 *
 * - `ok` — free space ≥ threshold; `free_mib` is reported for the UI.
 * - `below_threshold` — free space dipped below `threshold_mib`. The
 *   recording-start gate shows a warning modal; the app-mount banner
 *   surfaces a Sonner toast.
 */
export type DiskCheckOutcome =
  | { kind: "ok"; free_mib: number }
  | { kind: "below_threshold"; free_mib: number; threshold_mib: number };

/**
 * Discriminator for the `tray:degraded` event payload. Mirrors the
 * Rust `DegradedKind` enum — three failure modes documented in
 * `docs/plan.md` week 12.
 */
export type DegradedKind = "tap_lost" | "ax_unavailable" | "aec_overflow";

/**
 * Wire-format payload for the `tray:degraded` event. Mirrors the Rust
 * `DegradedPayload` struct.
 */
export interface DegradedPayload {
  kind: DegradedKind;
  at_secs: number;
  /** Optional target-app label, e.g. "Zoom". */
  target: string | null;
  /**
   * Active recording's session id, set by the FSM dispatch path
   * (post-pipeline-integration). The toast's "View diagnostics"
   * action uses this as the navigation target. `null` / undefined →
   * the toast falls back to "newest note in the vault".
   */
  session_id?: string | null;
}

/**
 * Wire shape returned by `heron_disk_usage`. Mirrors the Rust
 * `DiskUsage` struct in `apps/desktop/src-tauri/src/disk.rs`.
 */
export interface DiskUsage {
  vault_bytes: number;
  vault_session_count: number;
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

/**
 * One row in the crash-recovery salvage list. Mirrors the Rust
 * `UnfinalizedSession` struct in `salvage.rs`.
 */
export interface UnfinalizedSession {
  session_id: string;
  /** ISO 8601 / RFC 3339 timestamp string. */
  started_at: string;
  audio_bytes: number;
  has_partial_transcript: boolean;
}

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
  /**
   * Phase 67 (PR-ε): platform default cache root
   * (`~/Library/Caches/com.heronnote.heron` on macOS). The Review
   * playback bar passes the returned string into
   * `heron_resolve_recording` so the asset-protocol resolver can
   * locate per-session WAV mixdowns when the m4a hasn't been encoded
   * yet.
   */
  heron_default_cache_root: {
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
  /**
   * Phase 67 (PR-ε): re-summarize an existing note in place. The vault
   * writer rotates the prior body into `<id>.md.bak` before
   * overwriting; the rendered new note (frontmatter + body) is
   * returned so the editor can re-mount immediately.
   */
  heron_resummarize: {
    args: { vaultPath: string; sessionId: string };
    returns: string;
  };
  /**
   * Phase 76 (PR-ξ): preview the post-merge note for the diff modal.
   * Runs the same summarize + §10.3 merge pipeline as
   * `heron_resummarize` but never writes `<id>.md` and never rotates
   * `<id>.md.bak`. The Review UI compares the returned string against
   * the current `<id>.md` and the user clicks Apply (which fires
   * `heron_resummarize`) or Cancel.
   */
  heron_resummarize_preview: {
    args: { vaultPath: string; sessionId: string };
    returns: string;
  };
  /**
   * Phase 67 (PR-ε): report whether a `<id>.md.bak` is present. `null`
   * when there's no backup — the Review UI hides the Restore pill.
   */
  heron_check_backup: {
    args: { vaultPath: string; sessionId: string };
    returns: BackupInfo | null;
  };
  /**
   * Phase 67 (PR-ε): restore `<id>.md` from `<id>.md.bak` and delete
   * the backup. Returns the restored body so the editor re-mounts
   * immediately.
   */
  heron_restore_backup: {
    args: { vaultPath: string; sessionId: string };
    returns: string;
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
   * Phase 71 (PR-ι): persist the "wizard finished" flag on the
   * §13.3 onboarding wizard's "Finish setup" button. Idempotent —
   * the Rust side reads, flips, writes; re-running is a no-op.
   * Takes no path argument because the path is canonical
   * (`default_settings_path()`); see `heron_mark_onboarded`'s
   * doc comment in `lib.rs` for the rationale.
   */
  heron_mark_onboarded: {
    args: Record<string, never>;
    returns: void;
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
  /**
   * Phase 69 (PR-η): walk the cache root for unfinalized sessions.
   * Missing cache root resolves as an empty list, not a rejection.
   */
  heron_scan_unfinalized: {
    args: Record<string, never>;
    returns: UnfinalizedSession[];
  };
  /**
   * Phase 69 (PR-η): re-run finalize on a salvaged session and write
   * the resulting `.md` into `vaultPath`. Currently rejects with
   * "recovery is not yet wired through the orchestrator" — the
   * orchestrator's re-finalize entry point lands in a follow-up.
   */
  heron_recover_session: {
    args: { sessionId: string; vaultPath: string };
    returns: string;
  };
  /**
   * Phase 69 (PR-η): recursively delete the session's cache
   * directory. Refuses to follow symlinks. Validates the session id
   * is a basename (no `..`, no path separators).
   */
  heron_purge_session: {
    args: { sessionId: string };
    returns: void;
  };
  /**
   * Phase 69 (PR-η): basename of the newest `*.md` in the user's
   * configured vault, or `null` when the vault is empty / unset.
   */
  heron_last_note_session_id: {
    args: Record<string, never>;
    returns: string | null;
  };
  /**
   * Phase 68 (PR-ζ): register the system-wide Start/Stop Recording
   * hotkey. Errors carry a human-facing reason ("another app already
   * owns this chord"); the Settings pane surfaces them inline.
   */
  heron_register_hotkey: {
    args: { combo: string };
    returns: void;
  };
  /**
   * Phase 68 (PR-ζ): probe whether `combo` would conflict with an
   * existing system-wide hotkey. `true` means "free to register".
   */
  heron_check_hotkey: {
    args: { combo: string };
    returns: boolean;
  };
  /** Phase 68 (PR-ζ): release a previously-registered hotkey. */
  heron_unregister_hotkey: {
    args: { combo: string };
    returns: void;
  };
  /**
   * Phase 68 (PR-ζ): vault disk-usage gauge for the Audio tab.
   * Returns total bytes + session count at the vault root.
   */
  heron_disk_usage: {
    args: { vaultPath: string };
    returns: DiskUsage;
  };
  /**
   * Phase 68 (PR-ζ): purge `.wav` / `.m4a` audio sidecars whose mtime
   * is older than `days`. Returns the count actually deleted.
   */
  heron_purge_audio_older_than: {
    args: { vaultPath: string; days: number };
    returns: number;
  };
  /**
   * Phase 73 (PR-λ): pre-flight disk-space gate. Reads
   * `min_free_disk_mib` from the user's settings.json, asks the OS
   * how much free space the cache volume has, and returns the
   * decision as a discriminated union. The recording-start gate
   * (Home page) and the app-mount banner (App.tsx) both call this.
   */
  heron_check_disk_for_recording: {
    args: { settingsPath: string };
    returns: DiskCheckOutcome;
  };
  /**
   * Phase 73 (PR-λ): manually fire a `tray:degraded` event. Today
   * this is the only path that produces the event — real wiring
   * lands when the FSM's `CaptureDegraded` dispatch is integrated
   * into the recording pipeline. Useful from devtools to verify the
   * tray + Sonner toast UX without the audio pipeline running.
   */
  heron_emit_capture_degraded: {
    args: {
      kind: DegradedKind;
      atSecs: number;
      target: string | null;
      sessionId: string | null;
    };
    returns: void;
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
