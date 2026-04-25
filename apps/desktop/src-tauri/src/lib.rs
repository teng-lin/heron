//! Tauri v2 desktop shell — v0 scaffold.
//!
//! The full 5-step onboarding + recording UX + review UI lands
//! across §13–§16. This module ships:
//!
//! - the Tauri builder bootstrap (no-op for plugins; the audio /
//!   speech / vault wiring goes in once `heron-cli::session`
//!   gains a `run()` async path),
//! - one demonstrative `heron_status` command that returns the
//!   FSM state + a few orchestrator-side flags so the frontend
//!   has something concrete to render before week 11,
//! - the §15.2 asset-protocol resolver + §15.4 diagnostics +
//!   §16.1 settings persistence backends so the review UI and
//!   Settings pane can land in week 13/14 against stable Rust.

pub mod asset_protocol;
pub mod diagnostics;
pub mod onboarding;
pub mod settings;

use std::path::{Path, PathBuf};

use serde::Serialize;

pub use asset_protocol::{AssetError, AssetSource, resolve_recording_uri};
pub use diagnostics::{DiagnosticsError, DiagnosticsView, SessionLog, read_diagnostics};
pub use onboarding::{
    TestOutcome, test_accessibility, test_audio_tap, test_calendar, test_microphone,
    test_model_download,
};

// Tauri's command-handler macro requires the function names it
// generates wrappers for to live at the same path the macro is in;
// we keep the underlying free functions in `onboarding` for direct
// unit-testing, and the `#[tauri::command]` shims below thread the
// arguments through.
pub use settings::{Settings, SettingsError, read_settings, write_settings};

#[derive(Debug, Clone, Serialize)]
pub struct HeronStatus {
    pub version: String,
    /// Serializes via `RecordingState`'s `#[serde(rename_all =
    /// "snake_case")]`, so the wire format matches what the
    /// frontend already parses ("idle" / "armed" / etc.).
    pub fsm_state: heron_types::RecordingState,
    pub audio_available: bool,
    pub ax_backend: String,
}

#[tauri::command]
fn heron_status() -> HeronStatus {
    let fsm = heron_types::RecordingFsm::new();
    HeronStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        fsm_state: fsm.state(),
        // Once heron-audio's real capture lands, this will probe the
        // process tap permissions; v0 reports the stub state.
        audio_available: false,
        ax_backend: "ax-observer".into(),
    }
}

/// Tauri command: resolve `heron://recording/<id>` to a concrete asset.
///
/// Errors map to `String` so they reach the frontend without the
/// frontend needing the `AssetError` type — the UI distinguishes the
/// "missing" / "partial" cases by inspecting the message.
#[tauri::command]
fn heron_resolve_recording(
    session_id: String,
    m4a_candidate: String,
    cache_root: String,
) -> Result<AssetSource, String> {
    resolve_recording_uri(
        &session_id,
        Path::new(&m4a_candidate),
        Path::new(&cache_root),
    )
    .map_err(|e| e.to_string())
}

/// Tauri command: read `heron_session.json` and return the diagnostics
/// view.
#[tauri::command]
fn heron_diagnostics(session_log_path: String) -> Result<DiagnosticsView, String> {
    read_diagnostics(Path::new(&session_log_path)).map_err(|e| e.to_string())
}

/// Tauri command: load user settings.
#[tauri::command]
fn heron_read_settings(settings_path: String) -> Result<Settings, String> {
    read_settings(Path::new(&settings_path)).map_err(|e| e.to_string())
}

/// Tauri command: persist user settings.
#[tauri::command]
fn heron_write_settings(settings_path: String, settings: Settings) -> Result<(), String> {
    write_settings(Path::new(&settings_path), &settings).map_err(|e| e.to_string())
}

/// Tauri command: §13.3 step 1 microphone Test button.
#[tauri::command]
fn heron_test_microphone() -> TestOutcome {
    test_microphone()
}

/// Tauri command: §13.3 step 2 system-audio Test button.
#[tauri::command]
fn heron_test_audio_tap(target_bundle_id: String) -> TestOutcome {
    test_audio_tap(&target_bundle_id)
}

/// Tauri command: §13.3 step 3 accessibility Test button.
#[tauri::command]
fn heron_test_accessibility() -> TestOutcome {
    test_accessibility()
}

/// Tauri command: §13.3 step 4 calendar Test button.
#[tauri::command]
fn heron_test_calendar() -> TestOutcome {
    test_calendar()
}

/// Tauri command: §13.3 step 5 model-download Test button.
#[tauri::command]
fn heron_test_model_download(progress: f32) -> TestOutcome {
    test_model_download(progress)
}

/// Default settings location.
///
/// Resolves via [`dirs::config_dir`] so the path is correct on every
/// platform Tauri targets:
/// - macOS: `~/Library/Application Support/com.heronnote.heron/settings.json`
/// - Linux: `$XDG_CONFIG_HOME/com.heronnote.heron/settings.json`
/// - Windows: `%APPDATA%\com.heronnote.heron\settings.json`
///
/// v1 ships macOS-only, but keeping this portable means `cargo test`
/// runs the same path-resolution code on Linux CI runners and on a
/// future Windows build without surprise. Falls back to `./` if the
/// platform's config dir cannot be resolved (sandboxed test runners,
/// minimal containers).
pub fn default_settings_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("com.heronnote.heron").join("settings.json")
}

/// Entry point used by `main.rs` and (eventually) by Tauri's mobile
/// build target. The function name + `#[cfg_attr(...)]` line below
/// are required by Tauri 2's mobile entry point glue.
///
/// Panics if the Tauri context fails to start. This is the right
/// failure mode at the binary entry point — there is nothing the
/// caller can do to recover, and the panic message lands in the
/// system log.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
#[allow(clippy::expect_used)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // No-op for v0; week 11 wires the capture + status
            // pipelines here.
            let _ = app;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            heron_status,
            heron_resolve_recording,
            heron_diagnostics,
            heron_read_settings,
            heron_write_settings,
            heron_test_microphone,
            heron_test_audio_tap,
            heron_test_accessibility,
            heron_test_calendar,
            heron_test_model_download,
        ])
        .run(tauri::generate_context!())
        .expect("error while running heron-desktop");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_path_ends_in_app_id_and_filename() {
        let p = default_settings_path();
        // The `dirs::config_dir()` prefix is platform-specific; assert
        // only the tail we control. (macOS adds `Application Support`,
        // Linux adds `.config`, Windows adds `AppData/Roaming`, etc.)
        assert!(p.ends_with("com.heronnote.heron/settings.json"));
    }
}
