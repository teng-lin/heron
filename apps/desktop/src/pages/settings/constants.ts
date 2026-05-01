/**
 * Constants shared across the Settings sub-tree.
 *
 * Lifted from the original `pages/Settings.tsx` so each tab module
 * can import only what it uses without re-creating the dropdown
 * option lists.
 */

import type { FileNamingPattern } from "../../lib/invoke";

/** Debounce window (ms) for auto-save after the last `update()` call. */
export const AUTOSAVE_DEBOUNCE_MS = 500;

/**
 * Wire-format strings for `Settings.llm_backend`. Mirrors the
 * `"anthropic" | "openai" | "claude_code_cli" | "codex_cli"` values the
 * Rust side accepts (see `settings.rs`). The desktop side honors the
 * user's choice via `heron_llm::parse_settings_backend` →
 * `select_summarizer_with_user_choice`; an unrecognized string routes
 * to `Preference::Auto`.
 *
 * Grouped visually by hosted-API vs local-CLI per the IA note in the
 * UX-redesign brief: the user reads "API providers" as one billing
 * model and "local CLI" as another.
 */
export const LLM_BACKENDS = [
  { value: "anthropic", label: "Anthropic API", group: "api" },
  { value: "openai", label: "OpenAI API", group: "api" },
  { value: "claude_code_cli", label: "Claude Code CLI", group: "cli" },
  { value: "codex_cli", label: "Codex CLI", group: "cli" },
] as const;

/**
 * Anthropic-API model dropdown options. Values are the wire-format
 * model IDs Anthropic's `messages` endpoint expects. The orchestrator
 * does not yet read this back from settings.json — phase 41 (#42)
 * wires backend selection via env vars; the Settings field exists so
 * the data is captured ahead of the orchestrator change.
 */
export const ANTHROPIC_MODELS = [
  { value: "claude-opus-4-5", label: "Claude Opus 4.5" },
  { value: "claude-sonnet-4-5", label: "Claude Sonnet 4.5" },
  { value: "claude-haiku-4-5", label: "Claude Haiku 4.5" },
] as const;

/**
 * Tier 1 — `Settings.file_naming_pattern` options. Wire values match the
 * Rust `FileNamingPattern` enum's `#[serde(rename_all = "snake_case")]`
 * variants. The vault writer's slug pipeline (Tier 4 #19, PR #168)
 * consumes the chosen pattern.
 *
 * `satisfies` pins each `value` to the imported `FileNamingPattern`
 * union so a typo here is a build error; `as const` keeps the array
 * immutable + narrows each entry's type to its literal so TS treats
 * the radio's `value` as the typed union, matching the
 * `ANTHROPIC_MODELS` / `LLM_BACKENDS` pattern in this file.
 */
export const FILE_NAMING_PATTERNS = [
  {
    value: "id",
    label: "UUID",
    description: "<uuid>.md — original convention; preserves vault on upgrade.",
  },
  {
    value: "date_slug",
    label: "Date + slug",
    description: "<YYYY-MM-DD>-<slug>.md — chronological + readable.",
  },
  {
    value: "slug",
    label: "Slug only",
    description: "<slug>.md — readable filename without date prefix.",
  },
] as const satisfies readonly {
  value: FileNamingPattern;
  label: string;
  description: string;
}[];

/**
 * Tier 1 — `Settings.shortcuts` action ids the renderer knows about.
 * `toggle_recording` is the only canonical action id today (defined in
 * `apps/desktop/src-tauri/src/shortcuts.rs::ACTION_TOGGLE_RECORDING`)
 * — the rest of the map is a free-form key/value editor so a future
 * orchestrator action can be bound without a UI change. Listed first
 * here so the "+ Add shortcut" preset surfaces it as a default option.
 */
export const KNOWN_SHORTCUT_ACTIONS = [
  {
    value: "toggle_recording",
    label: "Toggle recording (overrides Hotkey tab default)",
  },
] as const;

/**
 * Default retention window when the user flips the Audio retention
 * radio to "purge" without typing a number. 30 days is the §16.1
 * brief's example value — a month of audio retention is the median
 * ask from the design-partner interviews documented in
 * `docs/scope-fixes.md`.
 */
export const DEFAULT_RETENTION_DAYS = 30;

/** How often we re-poll the disk-usage gauge while the Audio tab is open. */
export const DISK_USAGE_POLL_MS = 5000;

/**
 * Quick-add presets for the "Recorded apps" picker. Bundle IDs are
 * macOS LSApplicationIdentifier values; the labels are user-facing.
 */
export const PRESET_BUNDLES = [
  { value: "us.zoom.xos", label: "Zoom" },
  { value: "com.microsoft.teams2", label: "Microsoft Teams" },
  { value: "com.google.Chrome", label: "Google Chrome" },
] as const;
