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

import type {
  AttachContextAck,
  AutoRecordAck,
  CalendarPage,
  CalendarQuery,
  DaemonAudioSource,
  DaemonResult,
  ListMeetingsPage,
  ListMeetingsQuery,
  Meeting,
  MeetingId,
  Platform,
  PreMeetingContextRequest,
  PrepareContextRequest,
  SetEventAutoRecordRequest,
  Summary,
  Transcript,
} from "./types";

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
   * onboarding wizard. The Rust struct's container-level
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
  /**
   * UI revamp PR 2: which companion mode the TitleBar pill is set to.
   * The Rust `ActiveMode` enum serializes to these lowercase strings
   * via `#[serde(rename_all = "lowercase")]`; pre-revamp settings.json
   * files default to `"clio"` via the container's `#[serde(default)]`.
   * Pinned by `active_mode_serializes_lowercase` Rust-side.
   */
  active_mode: ActiveMode;
  /** Tier 1 (Schema only). Vocabulary boost terms for the STT backend. */
  hotwords: string[];
  /** Tier 1. User self-context for the summarizer prompt template. */
  persona: Persona;
  /**
   * Tier 1. Vault writer slug strategy. Mirrors the Rust
   * `FileNamingPattern` enum's `#[serde(rename_all = "snake_case")]`
   * variants.
   */
  file_naming_pattern: FileNamingPattern;
  /** Tier 1. `null` means "keep all" — same semantics as `audio_retention_days`. */
  summary_retention_days: number | null;
  /** Tier 1. Replace participant names with `Speaker A/B/C` before LLM. */
  strip_names_before_summarization: boolean;
  /** Tier 1. Show menu-bar REC pill while recording. */
  show_tray_indicator: boolean;
  /** Tier 1. Auto-detect meeting apps launching and prime recording. */
  auto_detect_meeting_app: boolean;
  /** Tier 1. OpenAI model id; takes effect when Tier 2's OpenAI summarizer ships. */
  openai_model: string;
  /**
   * Tier 1. Custom global-shortcut bindings keyed by action id. Tier 4
   * wiring iterates this map at startup and registers each binding.
   */
  shortcuts: Record<string, string>;
}

export type ActiveMode = "clio" | "athena" | "pollux";

/**
 * Tier 1. User self-context for the summarizer prompt template. Mirrors
 * the Rust `Persona` struct. The Settings UI binds its three inputs
 * ("Your name" / "Your role" / "What you're working on") to these fields.
 */
export interface Persona {
  name: string;
  role: string;
  working_on: string;
}

/**
 * Tier 1. Vault writer slug strategy. Mirrors the Rust
 * `FileNamingPattern` enum's `#[serde(rename_all = "snake_case")]`
 * variants. Default `"id"` preserves the pre-Tier-1 `<uuid>.md`
 * convention for backward compat.
 */
export type FileNamingPattern = "id" | "date_slug" | "slug";

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
 * `docs/archives/plan.md` week 12.
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
 * Wire-format payload for the `model_download:progress` Tauri event
 * emitted by `heron_download_model`. Mirrors the Rust
 * `apps/desktop/src-tauri/src/model_download.rs::ProgressPayload`
 * struct. `fraction` is a clamped `[0.0, 1.0]` ratio — render at
 * 100x scale for a percentage bar.
 */
export interface ModelDownloadProgress {
  fraction: number;
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
 * Wire-format reply from `heron_daemon_status`. Mirrors the Rust
 * `apps/desktop/src-tauri/src/daemon.rs::DaemonStatus` struct:
 * `running` is the boolean liveness gate; `version` carries the
 * daemon's reported semver when it answered with a parseable body;
 * `error` carries the reqwest/parse error string when the probe could
 * not classify the response as a healthy daemon.
 */
export interface DaemonStatus {
  running: boolean;
  version: string | null;
  error: string | null;
}

/**
 * Severity of a single `heron-doctor` runtime-check entry. Mirrors the
 * Rust `runtime_checks::Severity` enum (snake_case via
 * `#[serde(rename_all = "snake_case")]`).
 */
export type RuntimeCheckSeverity = "pass" | "warn" | "fail";

/**
 * Wire-format mirror of the Rust `RuntimeCheckEntry` returned by
 * `heron_run_runtime_checks`. Gap #6: surfaces the consolidated doctor
 * preflight (ONNX runtime, Zoom process, keychain ACL on macOS, network
 * reachability) to the React onboarding wizard.
 *
 * `name` is one of the doctor's stable identifiers — `onnx_runtime`,
 * `zoom_process`, `keychain_acl`, `network_reachability` — and the
 * renderer switches on it to render per-check copy. The list of names
 * is **not** exhaustive on the wire side because the doctor's check
 * set may grow (a future RTC / EventKit probe lives behind the same
 * façade); unknown `name` values fall back to a generic row instead of
 * being filtered out.
 */
export interface RuntimeCheckEntry {
  name: string;
  severity: RuntimeCheckSeverity;
  summary: string;
  detail: string;
}

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
 * Synthetic ack the Tauri proxy returns for a successful end-meeting
 * call. Mirrors `apps/desktop/src-tauri/src/meetings.rs::EndMeetingAck`.
 *
 * The daemon's HTTP endpoint emits `204 No Content`; the proxy echoes
 * the validated meeting id back inside this struct so the JS side has a
 * typed handle to clear local recording state without re-deriving the
 * id from the request payload.
 */
export interface EndMeetingAck {
  meeting_id: MeetingId;
}

/**
 * Synthetic ack for `heron_pause_meeting` / `heron_resume_meeting`.
 * Mirrors `apps/desktop/src-tauri/src/meetings.rs::PauseMeetingAck`.
 *
 * Same rationale as [`EndMeetingAck`]: the daemon emits `204 No
 * Content`; the proxy echoes the validated meeting id back so the JS
 * side can flip local recording-store state without re-deriving the
 * id. Tier 3 #16.
 */
export interface PauseMeetingAck {
  meeting_id: MeetingId;
}

/**
 * Wire payload mirroring the Rust `shortcuts::ConflictNotice` struct
 * in `apps/desktop/src-tauri/src/shortcuts.rs`. Returned (one per
 * collision) by [`heron_take_pending_shortcut_conflicts`] and emitted
 * over the `shortcut:conflict` Tauri event for live conflicts.
 *
 * - `accelerator` — the chord both action ids parsed to.
 * - `kept` — the action id whose registration won (sorted-key first).
 * - `skipped` — the action id whose registration was dropped.
 *
 * The Settings pane renders these as one Sonner toast each so a user-
 * edited `settings.json` mistake surfaces without spamming the UI.
 */
export interface ShortcutConflictNotice {
  accelerator: string;
  kept: string;
  skipped: string;
}

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

/**
 * Day 8-10 write-back: per-row patch shape for
 * `heron_update_action_item`. Mirrors `heron_vault::ActionItemPatch`
 * (RFC 7396 JSON Merge Patch).
 *
 * Per-field semantics on the wire:
 * - **omit the key** to leave the field unchanged
 * - **set to `null`** (only valid for nullable fields `owner` / `due`)
 *   to clear the field
 * - **set to a value** to overwrite
 *
 * The Rust side uses a custom `Option<Option<T>>` deserializer to
 * distinguish missing-from-JSON from explicit `null`; serde's default
 * collapses both to `None` and would lose the "clear" signal. Stick
 * to literal omission for "no change" — `undefined` works in JSON
 * because `JSON.stringify` drops `undefined` keys, but mixing the two
 * forms across the renderer is a footgun.
 *
 * `text` and `done` are never nullable on the wire — `null` would be
 * meaningless for either. Pass a string / boolean to set, omit to
 * leave alone.
 */
export interface ActionItemPatch {
  /** New text body. Trim renderer-side; the writer rejects empty / whitespace-only. */
  text?: string;
  /** `null` to clear, string to set, omit to leave unchanged. */
  owner?: string | null;
  /** ISO `YYYY-MM-DD`. `null` to clear, omit to leave unchanged. */
  due?: string | null;
  /** New `done` flag. Only path that flips a user checkbox on disk. */
  done?: boolean;
}

/**
 * Issue #226: closed enum of frontend error classes the
 * `ErrorBoundary` may report. Mirrors the Rust
 * `apps/desktop/src-tauri/src/frontend_error.rs::ErrorClass`'s
 * `#[serde(rename_all = "snake_case")]` discriminants.
 *
 * Keeping this a closed union (rather than `string`) is the
 * Prometheus-cardinality safeguard: the metric label dimension
 * `error_class` only ever takes one of these four values. Adding a
 * new variant is intentionally a Rust + TS edit so the wire surface
 * stays auditable.
 */
export type FrontendErrorClass =
  | "render_error"
  | "lifecycle_error"
  | "promise_rejection"
  | "unknown";

/**
 * Issue #226: wire-format payload for `heron_report_frontend_error`.
 * Mirrors the Rust `FrontendErrorReport` struct in `frontend_error.rs`.
 *
 * **Privacy contract:** the renderer must build this from explicit
 * safe fields only — never `JSON.stringify(props)`, never
 * `serialize(state)`. See `buildFrontendErrorReport` in
 * `apps/desktop/src/lib/errorReport.ts` for the redactor and the
 * unit tests that pin the no-leak guarantee.
 *
 * - `message` — `error.message` body, truncated + home-dir-redacted.
 * - `component` — build-time component path (e.g. `"App.Recording"`).
 *   NOT a filesystem path. Doubles as the `component` metric label
 *   dimension after `RedactedLabel::hashed` on the Rust side.
 * - `route` — react-router `pathname` at the time of the error.
 *   Build-time strings only.
 * - `app_version` / `app_build` — `__APP_VERSION__` / `__APP_BUILD__`
 *   from `vite.config.ts`.
 * - `stack` / `component_stack` — optional strings with home-dir
 *   prefixes normalized to `~/`. The Rust side does NOT re-redact —
 *   the renderer is the source of truth for "this is safe to log."
 */
export interface FrontendErrorReport {
  error_class: FrontendErrorClass;
  message: string;
  component: string;
  route: string;
  app_version: string;
  app_build: string;
  stack: string | null;
  component_stack: string | null;
}

/**
 * Day 8-10 write-back: post-merge action-item row returned from
 * `heron_update_action_item`. Mirrors
 * `apps/desktop/src-tauri/src/action_items.rs::ActionItemView`.
 *
 * `owner` / `due` are nullable (matching the existing `lib/types.ts::ActionItem`
 * shape — `null` means "no value"); `done` is always emitted because
 * the Rust side has a definite post-merge value to return.
 */
export interface ActionItemView {
  id: string;
  text: string;
  owner: string | null;
  due: string | null;
  done: boolean;
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
   * Day 8-10 write-back: apply a per-row [`ActionItemPatch`] against
   * `<vault>/<meetingId>.md`'s `Frontmatter.action_items` and atomically
   * rewrite the note. Returns the post-merge row so the renderer can
   * drop optimistic UI without a follow-up `heron_get_meeting`.
   *
   * `meetingId` is the vault note's `<session_id>.md` basename — the
   * same id `heron_resummarize` consumes; named `meetingId` on the
   * wire to align with the daemon's `MeetingId` semantics.
   * `itemId` is the `Frontmatter.action_items[].id` UUID minted by
   * the vault writer (Tier 0 #3).
   *
   * Errors come back through the standard `Promise.reject(string)`
   * envelope with a stable prefix the renderer can pattern-match:
   * - `not_found: action item <id> not found in note frontmatter`
   * - `validation: ...` (bad UUID, non-ISO due, empty text)
   * - `vault_locked: ...` (atomic write failed — iCloud eviction, etc.)
   */
  heron_update_action_item: {
    args: {
      vaultPath: string;
      meetingId: string;
      itemId: string;
      patch: ActionItemPatch;
    };
    returns: ActionItemView;
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
   * Gap #5b: trigger the real WhisperKit model download. Replaces the
   * prior placeholder badge that only checked whether a model was
   * already on disk. Resolves to a human-readable success message
   * (e.g. "WhisperKit model ready") on success; rejects with a
   * stringified error on every failure mode (`NotYetImplemented`,
   * `ModelMissing`, `Unavailable`, `Failed`, `Io`).
   *
   * Progress ticks (0.0..1.0) are pushed over the
   * `model_download:progress` Tauri event with a
   * `{ fraction: number }` payload. The wizard renders the value
   * as a real progress bar.
   */
  heron_download_model: {
    args: Record<string, never>;
    returns: string;
  };
  /**
   * Gap #5: probe the in-process / loopback `herond` at `/v1/health`.
   * Pass when the daemon answered with a parseable health body; fail
   * with a human-readable reason otherwise. The onboarding wizard's
   * 6th step calls this and gates "Finish setup" on a pass.
   */
  heron_test_daemon: {
    args: Record<string, never>;
    returns: TestOutcome;
  };
  /**
   * Gap #5: structured daemon status for surfaces that want
   * "running / version / error" without the `TestOutcome` lossy
   * collapse. Currently unused on the JS side but kept on the IPC
   * surface so a future menubar/status pill (or a status hook polling
   * on a timer) has a typed entry point and the Rust registration is
   * mirrored in the TS command map. Like every Tauri command this is
   * a request/response round-trip; a true push-based status feed
   * would ship as a separate Tauri event.
   */
  heron_daemon_status: {
    args: Record<string, never>;
    returns: DaemonStatus;
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
   * Reveal the user's vault folder in Finder. The Rust side reads
   * `vault_root` from the settings file at `settingsPath` rather than
   * trusting a renderer-supplied path, then validates it exists and is
   * a directory. macOS-only in v1; non-mac builds reject.
   */
  heron_open_vault_folder: {
    args: { settingsPath: string };
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
   * Tier 4 #24: drain the buffer of [`ShortcutConflictNotice`]s
   * captured during the Tauri `setup` hook before the webview started
   * listening. The frontend calls this once on mount to surface a
   * one-shot toast for each conflict the user introduced by hand-
   * editing `settings.json`. Returns an empty list when there are no
   * pending conflicts; the second drain in the same launch is always
   * empty (semantics: "take pending", not "peek").
   *
   * Pairs with the `shortcut:conflict` Tauri event for live conflicts
   * after launch.
   */
  heron_take_pending_shortcut_conflicts: {
    args: Record<string, never>;
    returns: ShortcutConflictNotice[];
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
   * Issue #226: report a frontend render-time error to the daemon.
   *
   * Called by `ErrorBoundary` (and by the `unhandledrejection` handler)
   * fire-and-forget. The Rust handler bumps
   * `frontend_errors_total{component, error_class}` on the same
   * Prometheus recorder #223 installed and logs the structured payload
   * via `tracing::warn!`. The renderer constructs `report` from
   * explicit safe fields only — see `lib/errorReport.ts` for the
   * redactor and the no-leak unit test.
   *
   * Resolves to `void` on success. Errors are swallowed Rust-side and
   * returned as a stringified rejection; callers should `.catch()` to
   * a no-op so the ErrorBoundary UI keeps rendering when the daemon is
   * down.
   */
  heron_report_frontend_error: {
    args: { report: FrontendErrorReport };
    returns: void;
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
   * Tier 4 #20: purge `.md` summary files whose mtime is older than
   * `days`. Returns the count actually deleted. Sibling of
   * `heron_purge_audio_older_than`; consumes
   * `Settings.summary_retention_days`. The audio sidecars are never
   * candidates — the two sweepers operate on disjoint extension sets.
   */
  heron_purge_summaries_older_than: {
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
  /**
   * Gap #6: run `heron-doctor`'s consolidated runtime preflight (ONNX
   * runtime health, Zoom process availability, keychain ACL on macOS,
   * network reachability) and return one entry per check. Used by the
   * onboarding wizard's "Runtime checks" step to surface the
   * cross-cutting "is this machine ready to record?" verdict in one
   * call rather than the renderer fanning out across the four
   * underlying probes.
   *
   * Resolves with the entry list even when individual checks fail — a
   * `fail` severity entry is the success path on the wire. Rejects
   * only when the underlying `spawn_blocking` task panics, which is a
   * pure infrastructure error.
   */
  heron_run_runtime_checks: {
    args: Record<string, never>;
    returns: RuntimeCheckEntry[];
  };
  /**
   * UI revamp PR 3: proxy `GET /v1/meetings`. Returns the
   * `DaemonResult<ListMeetingsPage>` discriminated union so the
   * frontend Zustand store can branch on transport failure (the
   * daemon-down banner) without parsing error strings.
   */
  heron_list_meetings: {
    args: { query: ListMeetingsQuery };
    returns: DaemonResult<ListMeetingsPage>;
  };
  /**
   * Gap #8 follow-up: proxy `GET /v1/meetings/{id}` so Review can
   * render canonical daemon metadata instead of treating the route
   * param as the whole meeting model.
   */
  heron_get_meeting: {
    args: { meetingId: MeetingId };
    returns: DaemonResult<Meeting>;
  };
  /**
   * UI revamp PR 3: proxy `GET /v1/meetings/{id}/summary`. Used by
   * the Home page's lazy-preview hover to render the first ~120
   * chars of the meeting summary.
   */
  heron_meeting_summary: {
    args: { meetingId: string };
    returns: DaemonResult<Summary>;
  };
  /**
   * Gap #8 follow-up: proxy `GET /v1/meetings/{id}/transcript`.
   * Review uses this for finalized daemon transcripts; live partials
   * still arrive over SSE.
   */
  heron_meeting_transcript: {
    args: { meetingId: MeetingId };
    returns: DaemonResult<Transcript>;
  };
  /**
   * Gap #8 follow-up: proxy `GET /v1/meetings/{id}/audio`. The Rust
   * command streams the daemon response into the app cache and returns
   * a local file path so the WebView can play it via `convertFileSrc`.
   */
  heron_meeting_audio: {
    args: { meetingId: MeetingId };
    returns: DaemonResult<DaemonAudioSource>;
  };
  /**
   * Gap #7 recording-capture wiring: proxy `POST /v1/meetings`. The
   * Home page's Start recording button funnels through here after
   * the consent gate. Returns the freshly-created Meeting on success
   * (the daemon's 202 body); on transport / 4xx / 5xx failure
   * collapses to `unavailable` with the error in `detail` so the UI
   * can toast and stay on /home rather than navigate into a
   * recording page with no meeting.
   *
   * `hint` and `calendarEventId` are optional — the daemon's
   * orchestrator uses them to disambiguate when multiple platforms
   * are running (hint), or to attach pre-meeting calendar context
   * (calendarEventId). v1 desktop callers leave both undefined and
   * default to Zoom; the picker / detection lands in a follow-up.
   */
  heron_start_capture: {
    args: {
      platform: Platform;
      hint?: string | null;
      calendarEventId?: string | null;
    };
    returns: DaemonResult<Meeting>;
  };
  /**
   * Gap #7 recording-capture wiring: proxy `POST /v1/meetings/{id}/end`.
   * The Recording page's Stop & Save button funnels through here.
   * The daemon emits `204 No Content`; this proxy synthesizes an
   * `EndMeetingAck { meeting_id }` so the JS side has a typed handle
   * to clear local recording state on success.
   */
  heron_end_meeting: {
    args: { meetingId: MeetingId };
    returns: DaemonResult<EndMeetingAck>;
  };
  /**
   * Tier 3 #16: proxy `POST /v1/meetings/{id}/pause`. The Recording
   * page's Pause button funnels through here so the daemon-side
   * capture pipeline actually drops audio frames (previously the
   * button only flipped local React state, and frames kept landing
   * on disk). The daemon emits `204 No Content`; the proxy synthesizes
   * a `PauseMeetingAck { meeting_id }` so the JS side has a typed
   * handle without parsing an empty body.
   */
  heron_pause_meeting: {
    args: { meetingId: MeetingId };
    returns: DaemonResult<PauseMeetingAck>;
  };
  /**
   * Tier 3 #16: proxy `POST /v1/meetings/{id}/resume`. Counterpart to
   * `heron_pause_meeting` — flips a paused capture back to recording.
   */
  heron_resume_meeting: {
    args: { meetingId: MeetingId };
    returns: DaemonResult<PauseMeetingAck>;
  };
  /**
   * Gap #8: proxy `GET /v1/calendar/upcoming`. Powers the Home page's
   * upcoming-meetings rail. The daemon's orchestrator reads from
   * EventKit via `heron_vault::CalendarReader`; this proxy is the
   * only way the webview can reach that data without the renderer
   * being granted raw EventKit access.
   *
   * `from` / `to` are optional RFC 3339 strings — the daemon defaults
   * to `now` → `now + 7 days` when omitted. `limit` is capped at 100
   * server-side.
   */
  heron_list_calendar_upcoming: {
    args: { query: CalendarQuery };
    returns: DaemonResult<CalendarPage>;
  };
  /**
   * Gap #8: proxy `PUT /v1/context`. Pre-stages agenda + attendees +
   * briefing so the orchestrator finds them in `pending_contexts`
   * (keyed by `calendar_event_id`) when the matching `start_capture`
   * fires. The daemon emits `204 No Content`; this proxy synthesizes
   * an `AttachContextAck { calendar_event_id }` so the JS side has a
   * typed handle without parsing an empty body.
   */
  heron_attach_context: {
    args: { request: PreMeetingContextRequest };
    returns: DaemonResult<AttachContextAck>;
  };
  /**
   * Tier 5 #25: proxy `POST /v1/context/prepare`. Auto-stages a
   * minimal default `PreMeetingContext` (today: just
   * `attendees_known`) so the rail can render a "primed" indicator on
   * each event card. Idempotent on the daemon side: never overwrites
   * a context attached manually via `heron_attach_context`.
   */
  heron_prepare_context: {
    args: { request: PrepareContextRequest };
    returns: DaemonResult<AttachContextAck>;
  };
  /**
   * Tier 5 #26: proxy `POST /v1/auto-record`. Toggles the daemon's
   * per-event auto-record registry; future calendar loads mirror the
   * flag onto `CalendarEvent.auto_record`.
   */
  heron_set_event_auto_record: {
    args: { request: SetEventAutoRecordRequest };
    returns: DaemonResult<AutoRecordAck>;
  };
  /**
   * UI revamp PR 4: ensure the Tauri-side SSE bridge is running.
   * Idempotent — `useSseEvents` calls this on mount; multiple
   * subscribers share one bridge.
   */
  heron_subscribe_events: {
    args: Record<string, never>;
    returns: void;
  };
  /**
   * UI revamp PR 4: cancel the SSE bridge. Called from the
   * `RunEvent::Exit` hook in Rust; the React side normally doesn't
   * call this directly.
   */
  heron_unsubscribe_events: {
    args: Record<string, never>;
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

/**
 * Day 8-10 write-back: thin wrapper around the typed `invoke` so the
 * Review tab's optimistic-UI handlers don't have to spell out the
 * command name. Returns the post-merge action-item row; rejects with
 * the `not_found:` / `validation:` / `vault_locked:` envelope strings
 * the renderer pattern-matches on.
 *
 * Pass `patch` fields with the JSON Merge Patch (RFC 7396) convention:
 * omit a key for "no change", set to `null` (only on `owner` / `due`)
 * for "clear", set to a value for "set". See [`ActionItemPatch`] for
 * the per-field rules.
 */
export async function updateActionItem(args: {
  vaultPath: string;
  meetingId: string;
  itemId: string;
  patch: ActionItemPatch;
}): Promise<ActionItemView> {
  return invoke("heron_update_action_item", args);
}
