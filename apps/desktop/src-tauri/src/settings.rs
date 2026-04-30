//! Settings pane backend per §16.1.
//!
//! Persists ~10 user preferences to a JSON file on disk. Exposes
//! `read_settings(path)` and `write_settings(path, &settings)` to be
//! wrapped as Tauri commands.
//!
//! The on-disk shape is intentionally one big JSON object rather than
//! a struct-of-structs: settings drift in over time and a flat shape
//! tolerates additions without migration ceremony. Unknown fields on
//! read are ignored; missing fields fall back to defaults.
//!
//! Atomicity follows `heron-vault::atomic_write` in spirit: write to
//! a temp file in the same directory, fsync, then rename. Same recipe,
//! kept local so the desktop crate doesn't pull a heron-vault dep.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::{NoContext, Timestamp, Uuid};

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("settings.json could not be parsed: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Companion-mode taxonomy per the v2 architecture pivot
/// (see `docs/heron-vision.md`). The TitleBar's mode pill flips this
/// field; the rest of the UI gates Athena/Pollux affordances on it.
///
/// Default: [`ActiveMode::Clio`] — the silent note-taker, which is
/// the only mode shipping today. Athena and Pollux ship later but
/// are routable stubs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ActiveMode {
    #[default]
    Clio,
    Athena,
    Pollux,
}

/// User self-context the summarizer can inject into the LLM prompt
/// (Tier 4 wiring). Three discrete inputs so the Settings UI's
/// "Your name" / "Your role" / "What you're working on" fields can
/// bind to named struct members rather than parsing a free-form string.
///
/// The container-level `#[serde(default)]` makes each field optional on
/// read so a partially hand-edited `settings.json` (e.g. only `name`
/// present) deserializes cleanly rather than hard-erroring on the missing
/// sibling fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Persona {
    pub name: String,
    pub role: String,
    pub working_on: String,
}

/// Vault writer slug strategy (Tier 4 wiring). Controls how the
/// `.md` filename is derived for each session.
///
/// Default: [`FileNamingPattern::Id`] — preserves the pre-Tier-1
/// `<uuid>.md` convention so existing vaults are unaffected on upgrade.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileNamingPattern {
    /// `<uuid>.md` — the original naming convention (backward compat).
    #[default]
    Id,
    /// `<YYYY-MM-DD>-<slug>.md` — date-prefixed human-readable slug.
    DateSlug,
    /// `<slug>.md` — human-readable slug only.
    Slug,
}

/// Tier 4 #19 hand-off to the writer-side enum. Both enums carry the
/// same wire format (`"id"` / `"date_slug"` / `"slug"`) and the same
/// variant set; this `From` impl is the only seam the orchestrator
/// boot path uses to convert the persisted settings value into the
/// pattern the vault writer consumes. Pinned by
/// `file_naming_pattern_converts_into_vault_enum_one_to_one`.
impl From<FileNamingPattern> for heron_vault::FileNamingPattern {
    fn from(value: FileNamingPattern) -> Self {
        match value {
            FileNamingPattern::Id => heron_vault::FileNamingPattern::Id,
            FileNamingPattern::DateSlug => heron_vault::FileNamingPattern::DateSlug,
            FileNamingPattern::Slug => heron_vault::FileNamingPattern::Slug,
        }
    }
}

/// User-facing settings persisted by the Settings pane.
///
/// Defaults match the §16.1 v1 starting values: STT runs WhisperKit
/// with the small/quantized variant, LLM via Anthropic, summary on
/// stop. The Settings pane lets the user override each.
///
/// ## Forward-compat / migration
///
/// Every field carries `#[serde(default)]` so an on-disk `settings.json`
/// written by an older heron build (missing fields the current build
/// added) deserializes cleanly with the missing fields filled by
/// [`Settings::default`]. Without it, `serde_json::from_slice` would
/// reject the file with `missing field 'audio_retention_days'` (PR-ζ
/// added that field in phase 68) and the Settings pane would refuse
/// to load. The `default` attribute is paired with a per-field
/// `default = "..."` only where the field's "missing" value differs
/// from `Default::default()`'s; in every case here the struct-level
/// `Default` impl already provides the right fallback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// `"whisperkit"` or `"sherpa"`. STT backend selection per §8.6.
    pub stt_backend: String,
    /// `"anthropic"`, `"claude_code_cli"`, `"codex_cli"`. LLM backend
    /// selection per §11.1.
    pub llm_backend: String,
    /// Auto-summarize on stop, or wait for an explicit request from
    /// the review UI?
    pub auto_summarize: bool,
    /// Path to the Obsidian vault root. Empty string means "ask on
    /// next session" (the onboarding flow walks the user through
    /// picking it the first time).
    pub vault_root: String,
    /// Hotkey string per Tauri's `tauri-plugin-global-shortcut` syntax
    /// (e.g. `"CmdOrCtrl+Shift+R"`).
    pub record_hotkey: String,
    /// Disclosure-banner remind interval in seconds (§14.2).
    pub remind_interval_secs: u32,
    /// Whether crash-recovery scan runs on app launch (§14.1).
    pub recover_on_launch: bool,
    /// Disk-space threshold in MiB below which recording is disabled
    /// (§14.1, §18.1).
    pub min_free_disk_mib: u32,
    /// Whether to emit per-session log lines (§19.2).
    pub session_logging: bool,
    /// Send local diagnostics on crash? Off by default.
    pub crash_telemetry: bool,
    /// Phase 68 (PR-ζ): how long to keep `.wav` / `.m4a` audio files
    /// next to a session's `.md` summary.
    ///
    /// `None` means "keep all" — the Audio tab's "Keep all" radio.
    /// `Some(N)` means "purge audio whose mtime is older than N days".
    /// The transcript and summary `.md` are never purged regardless of
    /// the setting; only the lossy/lossless audio sidecars are
    /// candidates. The actual purge is driven by
    /// [`crate::disk::purge_audio_older_than`], which the Settings
    /// pane's "Purge now" button + an eventual launch-time hook call.
    pub audio_retention_days: Option<u32>,
    /// Phase 71 (PR-ι): `true` once the user has finished the §13.3
    /// five-step onboarding wizard. Persisted so subsequent app
    /// launches skip the wizard and route straight to the home /
    /// last-note state (see `App.tsx`).
    ///
    /// The struct-level `#[serde(default)]` already covers the
    /// "missing field" migration path — pre-PR-71 settings.json files
    /// deserialize with `onboarded = false`, which means the wizard
    /// runs once after upgrade. The dedicated test
    /// `read_pre_phase_71_settings_fills_onboarded_default` pins that
    /// contract so a future regression that drops the container-level
    /// `default` doesn't silently re-onboard every existing user.
    pub onboarded: bool,
    /// Phase 73 (PR-λ): Bundle IDs the user wants the Core Audio
    /// process tap to record. Defaults to the single Zoom desktop
    /// bundle ID for backward compatibility with PR-α; the Settings →
    /// Audio "Recorded apps" card lets the user add Microsoft Teams /
    /// Google Chrome / other meeting clients.
    ///
    /// `#[serde(default = "default_target_bundle_ids")]` lets pre-PR-λ
    /// settings.json files (which have no `target_bundle_ids` field)
    /// deserialize cleanly: missing → `["us.zoom.xos"]` → identical
    /// recording behavior to pre-phase-73 builds. The default is hand-
    /// rolled in a free function rather than relying on
    /// `Vec::default()` (the empty vec) — a missing field must
    /// reproduce the pre-PR-λ "Zoom is recorded" contract, not silently
    /// turn the user's existing recordings into mic-only sessions.
    #[serde(default = "default_target_bundle_ids")]
    pub target_bundle_ids: Vec<String>,
    /// UI revamp PR 2: which companion mode the TitleBar pill is
    /// currently set to. Drives sidebar / mode-gating behavior across
    /// the React tree. Pre-revamp settings.json files have no
    /// `active_mode` field; the container-level `#[serde(default)]`
    /// fills the missing field with `ActiveMode::Clio`, the only mode
    /// shipping today. Pinned by
    /// `read_pre_revamp_settings_fills_active_mode_default`.
    pub active_mode: ActiveMode,
    /// Vocabulary boost terms for the STT backend. WhisperKit's prompt
    /// hook (Tier 4 wiring; this PR only persists the field). Empty vec
    /// means "no hotwords supplied".
    pub hotwords: Vec<String>,
    /// User self-context the summarizer can inject into the LLM prompt
    /// (Tier 4 wiring). Persona is a struct so the Settings UI's three
    /// inputs ("Your name" / "Your role" / "What you're working on") can
    /// bind to discrete fields.
    pub persona: Persona,
    /// Vault writer slug strategy (Tier 4 wiring). Defaults to `Id` for
    /// backward compat with the current `<uuid>.md` convention.
    pub file_naming_pattern: FileNamingPattern,
    /// How long to keep summary `.md` files. `None` means "keep all"
    /// (matches the existing `audio_retention_days` semantics). Tier 4
    /// adds the sweeper that consumes this.
    pub summary_retention_days: Option<u32>,
    /// Strip participant names from the transcript before sending to the
    /// LLM. Privacy toggle. Tier 4 wiring replaces `participant.display_name`
    /// with `Speaker A/B/C` in the summarizer input pipeline.
    pub strip_names_before_summarization: bool,
    /// Show the menu-bar / dock REC pill while recording. Tier 4 wiring
    /// gates `tray.rs` rendering on this.
    pub show_tray_indicator: bool,
    /// Auto-detect a meeting app launching and prime recording. Tier 4
    /// wiring gates the detector loop in `heron-orchestrator` /
    /// `heron-zoom`.
    pub auto_detect_meeting_app: bool,
    /// OpenAI model id. Field exists pre-emptively for the OpenAI
    /// summarizer backend (Tier 2). Default mirrors the docs:
    /// `"gpt-4o-mini"`.
    pub openai_model: String,
    /// Custom global-shortcut bindings keyed by action id. Tier 4 wiring
    /// iterates the map at startup and registers each via
    /// `tauri-plugin-global-shortcut`. Use `BTreeMap<String, String>` so
    /// serde output is order-stable across writes.
    pub shortcuts: std::collections::BTreeMap<String, String>,
}

/// PR-λ default for [`Settings::target_bundle_ids`]: a one-item vec
/// containing the Zoom desktop bundle ID. Lifted out as a free function
/// so `#[serde(default = "...")]` can name it without serde's closure
/// gymnastics, and so `Settings::default()` and the migration path
/// share one source of truth.
pub fn default_target_bundle_ids() -> Vec<String> {
    vec!["us.zoom.xos".to_owned()]
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            stt_backend: "whisperkit".to_owned(),
            llm_backend: "anthropic".to_owned(),
            auto_summarize: true,
            vault_root: String::new(),
            record_hotkey: "CmdOrCtrl+Shift+R".to_owned(),
            remind_interval_secs: 30,
            recover_on_launch: true,
            min_free_disk_mib: 2048,
            session_logging: true,
            crash_telemetry: false,
            audio_retention_days: None,
            onboarded: false,
            target_bundle_ids: default_target_bundle_ids(),
            active_mode: ActiveMode::default(),
            hotwords: Vec::new(),
            persona: Persona::default(),
            file_naming_pattern: FileNamingPattern::Id,
            summary_retention_days: None,
            strip_names_before_summarization: false,
            show_tray_indicator: true,
            auto_detect_meeting_app: true,
            openai_model: "gpt-4o-mini".to_owned(),
            shortcuts: std::collections::BTreeMap::new(),
        }
    }
}

/// Mark the user as onboarded by reading `path`, flipping
/// [`Settings::onboarded`] to `true`, and writing it back atomically
/// (§13.3 / PR-ι).
///
/// Reading first (instead of writing
/// `Settings { onboarded: true, ..Default::default() }`) preserves
/// any other fields the user may have already changed via the
/// Settings pane between launching the app and finishing the wizard
/// — uncommon, but the read-modify-write keeps that race side
/// benign. Re-running on an already-onboarded settings file is a
/// no-op (idempotent).
///
/// ## Concurrency
///
/// The read-modify-write is **not** synchronized against a
/// concurrent [`write_settings`] from another Tauri command (e.g.
/// the Settings pane's auto-save). Today this is benign: the wizard
/// only runs on first launch, before any UI affordance to reach the
/// Settings pane exists. A future "re-run onboarding" entry from
/// Settings would open the lost-update window — the Settings pane's
/// pending `dirty` edits could be clobbered by the wizard's flush
/// here. The fix when that lands is a `Mutex<()>` shared across all
/// settings.json writers (or a `patch_field` command that flips
/// individual fields server-side without re-reading the whole file).
///
/// ## Parse-error recovery
///
/// If `path` exists but `serde_json::from_slice` rejects it (a
/// truncated write from a power loss, manual hand-edit), bubble the
/// error up so the frontend toasts. Falling back to
/// `Settings::default()` here would silently overwrite a recoverable
/// settings file with a wizard-finished blank slate — losing any
/// user preferences the Settings pane had persisted. The wizard's
/// `Finish setup` button surfaces the error so the user can choose
/// to manually fix the file (via Finder) or accept the loss with a
/// future "Reset settings" affordance.
pub fn mark_onboarded(path: &Path) -> Result<(), SettingsError> {
    let mut settings = read_settings(path)?;
    if settings.onboarded {
        return Ok(());
    }
    settings.onboarded = true;
    write_settings(path, &settings)
}

/// Read settings from `path`. Returns `Settings::default()` if the
/// file does not exist — first-run state is "everything default".
pub fn read_settings(path: &Path) -> Result<Settings, SettingsError> {
    // Single `fs::read`: dispatching off `std::io::ErrorKind::NotFound`
    // sidesteps the TOCTOU window an `exists()` pre-check would open
    // (the file could be deleted between the check and the read), and
    // `from_slice` skips the intermediate `String` copy.
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(SettingsError::Io(e)),
    }
}

/// Atomically write `settings` to `path`. Creates parent directories
/// if needed.
pub fn write_settings(path: &Path, settings: &Settings) -> Result<(), SettingsError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(settings)?;

    // UUID-named temp in the same directory so rename is atomic on
    // POSIX. Same pattern heron-vault::atomic_write uses; kept local
    // here so the desktop crate avoids a cyclic dep on heron-vault.
    let temp = settings_temp_path(path);
    {
        let mut f = File::create(&temp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    set_user_only_perms(&temp)?;
    fs::rename(&temp, path)?;
    Ok(())
}

fn settings_temp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // The workspace `uuid` only enables `v7`; v7 is monotonic +
    // unique-per-process, which is exactly what an atomic-rename
    // sidecar wants.
    parent.join(format!(
        ".heron-settings-{}.tmp",
        Uuid::new_v7(Timestamp::now(NoContext))
    ))
}

#[cfg(unix)]
fn set_user_only_perms(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_user_only_perms(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn first_run_returns_defaults() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        let settings = read_settings(&path).expect("read");
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        let settings = Settings {
            stt_backend: "sherpa".to_owned(),
            auto_summarize: false,
            vault_root: "/tmp/vault".to_owned(),
            remind_interval_secs: 60,
            crash_telemetry: true,
            audio_retention_days: Some(30),
            ..Default::default()
        };

        write_settings(&path, &settings).expect("write");
        let parsed = read_settings(&path).expect("read");
        assert_eq!(parsed, settings);
    }

    /// Older heron builds wrote a `settings.json` without the
    /// `audio_retention_days` field PR-ζ added. The `#[serde(default)]`
    /// container attribute on `Settings` must let those files load
    /// rather than failing the deserialize — otherwise the user's
    /// Settings pane breaks the first time they upgrade.
    #[test]
    fn read_pre_phase_68_settings_fills_audio_retention_default() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        // Verbatim copy of the field-set heron-desktop wrote before
        // phase 68. Note the absence of `audio_retention_days`.
        std::fs::write(
            &path,
            r#"{"stt_backend":"sherpa","llm_backend":"claude_code_cli","auto_summarize":false,
                "vault_root":"/tmp/vault","record_hotkey":"F12","remind_interval_secs":60,
                "recover_on_launch":false,"min_free_disk_mib":1024,"session_logging":false,
                "crash_telemetry":true}"#,
        )
        .expect("seed");
        let s = read_settings(&path).expect("read");
        assert_eq!(s.stt_backend, "sherpa");
        assert_eq!(s.audio_retention_days, None);
        // The non-default fields the file carried must survive the
        // partial deserialize untouched — the `#[serde(default)]`
        // container attribute fills *only* missing fields.
        assert_eq!(s.record_hotkey, "F12");
        assert!(s.crash_telemetry);
        assert!(!s.recover_on_launch);
        assert_eq!(s.min_free_disk_mib, 1024);
    }

    /// Conversely, a brand-new on-disk file with every field present
    /// (including `audio_retention_days`) must round-trip exactly.
    #[test]
    fn read_settings_with_audio_retention_some_round_trips() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        let s_in = Settings {
            audio_retention_days: Some(14),
            ..Default::default()
        };
        write_settings(&path, &s_in).expect("write");
        let s_out = read_settings(&path).expect("read");
        assert_eq!(s_out.audio_retention_days, Some(14));
    }

    #[test]
    fn write_creates_missing_parent_dir() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let nested = tmp.path().join("a/b/c/settings.json");
        write_settings(&nested, &Settings::default()).expect("nested write");
        assert!(nested.exists());
    }

    #[test]
    fn forward_compat_unknown_field_is_rejected_loudly() {
        // Settings is `Deserialize` without `#[serde(deny_unknown_fields)]`,
        // so an unknown key must round-trip silently. Future migrations
        // do the field-rename dance manually rather than relying on
        // schema strictness.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"stt_backend":"whisperkit","llm_backend":"anthropic","auto_summarize":true,
                "vault_root":"","record_hotkey":"CmdOrCtrl+Shift+R","remind_interval_secs":30,
                "recover_on_launch":true,"min_free_disk_mib":2048,"session_logging":true,
                "crash_telemetry":false,"audio_retention_days":null,"future_v2_field":"hello"}"#,
        )
        .expect("seed");
        let s = read_settings(&path).expect("read");
        assert_eq!(s.stt_backend, "whisperkit");
    }

    #[cfg(unix)]
    #[test]
    fn written_settings_have_user_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        write_settings(&path, &Settings::default()).expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// Pre-PR-71 settings.json files have no `onboarded` field. The
    /// container-level `#[serde(default)]` must let those deserialize
    /// cleanly with `onboarded = false`, so the wizard runs once on
    /// next launch instead of the deserialize failing and the user's
    /// Settings pane breaking. Sister test to
    /// `read_pre_phase_68_settings_fills_audio_retention_default`.
    #[test]
    fn read_pre_phase_71_settings_fills_onboarded_default() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        // Field-set heron-desktop wrote between PR-ζ (phase 68 added
        // `audio_retention_days`) and PR-ι (phase 71 added
        // `onboarded`). Note the absence of `onboarded`.
        std::fs::write(
            &path,
            r#"{"stt_backend":"whisperkit","llm_backend":"anthropic","auto_summarize":true,
                "vault_root":"/tmp/vault","record_hotkey":"CmdOrCtrl+Shift+R",
                "remind_interval_secs":30,"recover_on_launch":true,
                "min_free_disk_mib":2048,"session_logging":true,"crash_telemetry":false,
                "audio_retention_days":null}"#,
        )
        .expect("seed");
        let s = read_settings(&path).expect("read");
        assert!(
            !s.onboarded,
            "missing onboarded field must default to false"
        );
        // The other fields the file carried must survive untouched —
        // belt-and-suspenders against a regression that confuses
        // container-level default with field-level reset.
        assert_eq!(s.vault_root, "/tmp/vault");
        assert!(s.auto_summarize);
    }

    #[test]
    fn default_settings_have_onboarded_false() {
        // First-run state must be "not onboarded yet" so the wizard
        // routes (§13.3 / PR-ι) actually run for new installs.
        assert!(!Settings::default().onboarded);
    }

    #[test]
    fn mark_onboarded_flips_field_and_persists() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        // No file on disk yet — `mark_onboarded` reads via
        // `read_settings`, which yields `Settings::default()` for a
        // missing file, then writes back with `onboarded = true`.
        mark_onboarded(&path).expect("mark");
        let parsed = read_settings(&path).expect("read");
        assert!(parsed.onboarded);
    }

    #[test]
    fn mark_onboarded_preserves_other_fields() {
        // Writing `Settings { onboarded: true, ..Default::default() }`
        // would clobber any user customizations made between app
        // launch and finishing the wizard. `mark_onboarded` reads
        // first, then flips, so unrelated fields survive.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        let custom = Settings {
            vault_root: "/tmp/vault".to_owned(),
            record_hotkey: "F12".to_owned(),
            audio_retention_days: Some(14),
            ..Default::default()
        };
        write_settings(&path, &custom).expect("seed");
        mark_onboarded(&path).expect("mark");
        let parsed = read_settings(&path).expect("read");
        assert!(parsed.onboarded);
        assert_eq!(parsed.vault_root, "/tmp/vault");
        assert_eq!(parsed.record_hotkey, "F12");
        assert_eq!(parsed.audio_retention_days, Some(14));
    }

    #[test]
    fn mark_onboarded_is_idempotent() {
        // Re-running on an already-onboarded settings file is a no-op
        // write. The post-condition (`onboarded == true`) holds either
        // way; this test pins that the second call doesn't panic /
        // error / corrupt the file.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        mark_onboarded(&path).expect("mark 1");
        mark_onboarded(&path).expect("mark 2 (idempotent)");
        let parsed = read_settings(&path).expect("read");
        assert!(parsed.onboarded);
    }

    #[test]
    fn default_target_bundle_ids_is_zoom_only() {
        // PR-λ migration contract: the v1 single-target-bundle behavior
        // is reproduced by a one-item vec containing the Zoom desktop
        // bundle ID. The Settings → Audio "Recorded apps" card lets the
        // user add Teams / Chrome / etc., but the post-upgrade default
        // must record exactly what v1 recorded — anything else would
        // silently change recording semantics for existing users.
        let s = Settings::default();
        assert_eq!(s.target_bundle_ids, vec!["us.zoom.xos".to_owned()]);
    }

    #[test]
    fn target_bundle_ids_defaults_to_zoom_when_field_missing() {
        // A pre-PR-λ settings.json that predates the
        // `target_bundle_ids` field must deserialize cleanly with the
        // single-Zoom default. `Vec::default()` (the empty vec) would
        // silently break recording for every existing user — this test
        // pins the migration contract so a future regression that drops
        // `#[serde(default = "default_target_bundle_ids")]` (or that
        // accidentally relies on `Vec`'s default impl) fails loudly.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"stt_backend":"whisperkit","llm_backend":"anthropic","auto_summarize":true,
                "vault_root":"","record_hotkey":"CmdOrCtrl+Shift+R","remind_interval_secs":30,
                "recover_on_launch":true,"min_free_disk_mib":2048,"session_logging":true,
                "crash_telemetry":false,"audio_retention_days":null}"#,
        )
        .expect("seed");
        let s = read_settings(&path).expect("read");
        assert_eq!(s.target_bundle_ids, vec!["us.zoom.xos".to_owned()]);
    }

    #[test]
    fn target_bundle_ids_round_trips_with_multiple_apps() {
        // A user who has added Teams + Chrome via the Settings UI
        // should see all three round-trip through serialize/deserialize
        // unchanged, in declared order — order doubles as the user's
        // intended "primary target first" hint for the audio tap.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        let s = Settings {
            target_bundle_ids: vec![
                "us.zoom.xos".to_owned(),
                "com.microsoft.teams2".to_owned(),
                "com.google.Chrome".to_owned(),
            ],
            ..Default::default()
        };
        write_settings(&path, &s).expect("write");
        let parsed = read_settings(&path).expect("read");
        assert_eq!(parsed.target_bundle_ids, s.target_bundle_ids);
    }

    /// Pre-revamp settings.json files have no `active_mode` field. The
    /// container-level `#[serde(default)]` must let those deserialize
    /// cleanly with `active_mode = Clio`, so existing users land on the
    /// only-shipping mode rather than seeing the deserialize fail.
    /// Sister test to `read_pre_phase_71_settings_fills_onboarded_default`.
    #[test]
    fn read_pre_revamp_settings_fills_active_mode_default() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        // Field-set heron-desktop wrote between PR-λ (phase 73 added
        // `target_bundle_ids`) and the UI revamp (added `active_mode`).
        std::fs::write(
            &path,
            r#"{"stt_backend":"whisperkit","llm_backend":"anthropic","auto_summarize":true,
                "vault_root":"/tmp/vault","record_hotkey":"CmdOrCtrl+Shift+R",
                "remind_interval_secs":30,"recover_on_launch":true,
                "min_free_disk_mib":2048,"session_logging":true,"crash_telemetry":false,
                "audio_retention_days":null,"onboarded":true,
                "target_bundle_ids":["us.zoom.xos","com.microsoft.teams2"]}"#,
        )
        .expect("seed");
        let s = read_settings(&path).expect("read");
        assert_eq!(
            s.active_mode,
            ActiveMode::Clio,
            "missing active_mode field must default to Clio"
        );
        // Other fields the file carried must survive untouched —
        // belt-and-suspenders against a regression that confuses
        // container-level default with field-level reset.
        assert_eq!(s.vault_root, "/tmp/vault");
        assert!(s.onboarded);
        assert_eq!(
            s.target_bundle_ids,
            vec!["us.zoom.xos".to_owned(), "com.microsoft.teams2".to_owned(),]
        );
    }

    #[test]
    fn active_mode_round_trips_through_disk() {
        // The Settings pane's mode pill writes one of the three
        // variants; round-trip must preserve the choice across an app
        // restart so the user lands back on the mode they selected.
        for mode in [ActiveMode::Clio, ActiveMode::Athena, ActiveMode::Pollux] {
            let tmp = tempfile::TempDir::new().expect("tmp");
            let path = tmp.path().join("settings.json");
            let s_in = Settings {
                active_mode: mode,
                ..Default::default()
            };
            write_settings(&path, &s_in).expect("write");
            let s_out = read_settings(&path).expect("read");
            assert_eq!(s_out.active_mode, mode);
        }
    }

    #[test]
    fn active_mode_serializes_lowercase() {
        // The TS Settings interface in `lib/invoke.ts` declares
        // `active_mode: "clio" | "athena" | "pollux"`; the Rust enum
        // must serialize to those exact lowercase strings so the cross-
        // boundary contract holds without a manual mapping layer.
        let s = Settings {
            active_mode: ActiveMode::Athena,
            ..Default::default()
        };
        let json = serde_json::to_value(&s).expect("serialize");
        assert_eq!(json["active_mode"], "athena");
    }

    #[test]
    fn write_then_overwrite_replaces_atomically() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        let s1 = Settings {
            record_hotkey: "F11".to_owned(),
            ..Default::default()
        };
        write_settings(&path, &s1).expect("write 1");

        let s2 = Settings {
            record_hotkey: "F12".to_owned(),
            ..s1
        };
        write_settings(&path, &s2).expect("write 2");

        let parsed = read_settings(&path).expect("read");
        assert_eq!(parsed.record_hotkey, "F12");
    }

    /// Pre-Tier-1 settings.json files have none of the Tier-1 schema
    /// fields (hotwords, persona, file_naming_pattern,
    /// summary_retention_days, strip_names_before_summarization,
    /// show_tray_indicator, auto_detect_meeting_app, openai_model,
    /// shortcuts). The container-level `#[serde(default)]` must let those
    /// deserialize cleanly with `Settings::default()` filling each missing
    /// field — otherwise existing users' Settings panes break on upgrade.
    /// Sister test to `read_pre_revamp_settings_fills_active_mode_default`.
    #[test]
    fn read_pre_tier1_settings_fills_new_field_defaults() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        // Verbatim field-set heron-desktop wrote between the UI revamp PR
        // (added `active_mode`) and Tier 1. Note the absence of every new
        // field this PR adds.
        std::fs::write(
            &path,
            r#"{"stt_backend":"whisperkit","llm_backend":"anthropic","auto_summarize":true,
                "vault_root":"/tmp/vault","record_hotkey":"CmdOrCtrl+Shift+R",
                "remind_interval_secs":30,"recover_on_launch":true,
                "min_free_disk_mib":2048,"session_logging":true,"crash_telemetry":false,
                "audio_retention_days":null,"onboarded":true,
                "target_bundle_ids":["us.zoom.xos"],"active_mode":"clio"}"#,
        )
        .expect("seed");
        let s = read_settings(&path).expect("read");
        let defaults = Settings::default();
        assert_eq!(s.hotwords, defaults.hotwords);
        assert_eq!(s.persona, defaults.persona);
        assert_eq!(s.file_naming_pattern, defaults.file_naming_pattern);
        assert_eq!(s.summary_retention_days, defaults.summary_retention_days);
        assert_eq!(
            s.strip_names_before_summarization,
            defaults.strip_names_before_summarization
        );
        assert_eq!(s.show_tray_indicator, defaults.show_tray_indicator);
        assert_eq!(s.auto_detect_meeting_app, defaults.auto_detect_meeting_app);
        assert_eq!(s.openai_model, defaults.openai_model);
        assert_eq!(s.shortcuts, defaults.shortcuts);
        // Belt-and-suspenders: pre-Tier-1 fields the file carried must
        // survive untouched.
        assert_eq!(s.vault_root, "/tmp/vault");
        assert!(s.onboarded);
        assert_eq!(s.active_mode, ActiveMode::Clio);
    }

    /// Tier 4 #19 hand-off: every variant of the desktop-side
    /// `FileNamingPattern` must map 1:1 into the vault-writer enum.
    /// Adding a fourth variant on either side without updating both
    /// enums + this conversion is a silent break — pin it.
    #[test]
    fn file_naming_pattern_converts_into_vault_enum_one_to_one() {
        for (settings_pat, vault_pat) in [
            (FileNamingPattern::Id, heron_vault::FileNamingPattern::Id),
            (
                FileNamingPattern::DateSlug,
                heron_vault::FileNamingPattern::DateSlug,
            ),
            (
                FileNamingPattern::Slug,
                heron_vault::FileNamingPattern::Slug,
            ),
        ] {
            let converted: heron_vault::FileNamingPattern = settings_pat.into();
            assert_eq!(converted, vault_pat);
        }
    }

    /// `FileNamingPattern` variants must serialize to the exact
    /// `snake_case` string the TS type union declares (`"id"`,
    /// `"date_slug"`, `"slug"`). The Tauri IPC bridge forwards the
    /// raw JSON to the renderer, so any drift in variant wire names
    /// silently breaks the Settings UI's pattern picker.
    #[test]
    fn file_naming_pattern_serializes_to_snake_case_strings() {
        assert_eq!(
            serde_json::to_value(FileNamingPattern::Id).expect("ser"),
            "id"
        );
        assert_eq!(
            serde_json::to_value(FileNamingPattern::DateSlug).expect("ser"),
            "date_slug"
        );
        assert_eq!(
            serde_json::to_value(FileNamingPattern::Slug).expect("ser"),
            "slug"
        );
        // Round-trip all three through serde_json::from_value.
        for pat in [
            FileNamingPattern::Id,
            FileNamingPattern::DateSlug,
            FileNamingPattern::Slug,
        ] {
            let v = serde_json::to_value(pat).expect("ser");
            let back: FileNamingPattern = serde_json::from_value(v).expect("deser");
            assert_eq!(back, pat);
        }
    }

    /// A `persona` object with only some fields present (e.g. from a
    /// hand-edited `settings.json`) must deserialize cleanly — the
    /// `#[serde(default)]` on `Persona` fills the missing siblings with
    /// empty strings rather than hard-erroring.
    #[test]
    fn partial_persona_object_fills_defaults() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        // Only `name` present; `role` and `working_on` absent.
        std::fs::write(
            &path,
            r#"{"stt_backend":"whisperkit","llm_backend":"anthropic","auto_summarize":true,
                "vault_root":"","record_hotkey":"CmdOrCtrl+Shift+R","remind_interval_secs":30,
                "recover_on_launch":true,"min_free_disk_mib":2048,"session_logging":true,
                "crash_telemetry":false,"audio_retention_days":null,"onboarded":false,
                "target_bundle_ids":["us.zoom.xos"],"active_mode":"clio",
                "persona":{"name":"Alice"}}"#,
        )
        .expect("seed");
        let s = read_settings(&path).expect("read");
        assert_eq!(s.persona.name, "Alice");
        assert_eq!(s.persona.role, "");
        assert_eq!(s.persona.working_on, "");
    }

    /// Tier-1 fields must survive a full write→read round-trip with
    /// non-default values — exercises every new field path through the
    /// serde serializer and the atomic rename.
    #[test]
    fn tier1_fields_round_trip_non_default_values() {
        use std::collections::BTreeMap;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");
        let s_in = Settings {
            hotwords: vec!["heron".into()],
            persona: Persona {
                name: "Alice".into(),
                role: "PM".into(),
                working_on: "Q2 plan".into(),
            },
            file_naming_pattern: FileNamingPattern::Slug,
            summary_retention_days: Some(90),
            strip_names_before_summarization: true,
            show_tray_indicator: false,
            auto_detect_meeting_app: false,
            openai_model: "gpt-4o".into(),
            shortcuts: BTreeMap::from([("toggle_recording".into(), "F12".into())]),
            ..Default::default()
        };
        write_settings(&path, &s_in).expect("write");
        let s_out = read_settings(&path).expect("read");
        assert_eq!(s_out, s_in);
    }
}
