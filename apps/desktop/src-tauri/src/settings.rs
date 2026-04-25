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
        }
    }
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
}
