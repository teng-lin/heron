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
pub mod disk;
pub mod keychain;
pub mod notes;
pub mod onboarding;
pub mod preflight;
pub mod resummarize;
pub mod salvage;
pub mod settings;
pub mod tray;

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

pub use asset_protocol::{AssetError, AssetSource, resolve_recording_uri};
pub use diagnostics::{DiagnosticsError, DiagnosticsView, SessionLog, read_diagnostics};
pub use disk::{DiskError, DiskUsage, disk_usage, purge_audio_older_than};
pub use onboarding::{
    TestOutcome, test_accessibility, test_accessibility_async, test_audio_tap,
    test_audio_tap_async, test_calendar, test_calendar_async, test_microphone,
    test_microphone_async, test_model_download,
};
pub use preflight::{DiskCheckOutcome, check_disk, heron_check_disk_for_recording};

// Tauri's command-handler macro requires the function names it
// generates wrappers for to live at the same path the macro is in;
// we keep the underlying free functions in `onboarding` for direct
// unit-testing, and the `#[tauri::command]` shims below thread the
// arguments through.
pub use keychain::{KEYCHAIN_SERVICE, KeychainAccount, KeychainError};
pub use settings::{Settings, SettingsError, mark_onboarded, read_settings, write_settings};

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
        // Capability check (NOT session liveness): `true` iff this
        // host can in principle start a capture session — macOS with
        // a default cpal input device. TCC denial / target-app-not-
        // running surface as `Event::CaptureDegraded` on a real
        // session start, not here. See `heron_audio::audio_capture_available`
        // for the exact predicate, latency budget, and rationale.
        audio_available: heron_audio::audio_capture_available(),
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

/// Tauri command: resolve the platform-default settings.json path.
///
/// Returned as a `String` so the frontend can thread it back into
/// `heron_read_settings` / `heron_write_settings` without needing its
/// own `dirs::config_dir` resolver. Lossy UTF-8 conversion mirrors the
/// other path-returning surfaces — non-UTF-8 paths replace the offending
/// bytes with U+FFFD rather than fail. v1 ships macOS-only where this
/// can't happen in practice; the lossy fallback is belt-and-suspenders
/// for portability.
#[tauri::command]
fn heron_default_settings_path() -> String {
    default_settings_path().to_string_lossy().into_owned()
}

/// Tauri command: persist user settings.
#[tauri::command]
fn heron_write_settings(settings_path: String, settings: Settings) -> Result<(), String> {
    write_settings(Path::new(&settings_path), &settings).map_err(|e| e.to_string())
}

/// Tauri command: read `<vault>/<session_id>.md`.
///
/// `vault_path` and `session_id` are passed as separate strings so the
/// path policy enforced by `notes::read_note` (validate basename,
/// canonicalize, reject vault escapes) cannot be bypassed by
/// constructing the joined path on the renderer side.
#[tauri::command]
async fn heron_read_note(vault_path: String, session_id: String) -> Result<String, String> {
    notes::read_note(Path::new(&vault_path), &session_id).await
}

/// Tauri command: atomic-write `<vault>/<session_id>.md` (editor blur / ⌘S).
#[tauri::command]
async fn heron_write_note_atomic(
    vault_path: String,
    session_id: String,
    contents: String,
) -> Result<(), String> {
    notes::write_note_atomic(Path::new(&vault_path), &session_id, &contents).await
}

/// Tauri command: list `.md` session basenames in the vault directory.
#[tauri::command]
async fn heron_list_sessions(vault_path: String) -> Result<Vec<String>, String> {
    notes::list_sessions(Path::new(&vault_path)).await
}

/// Tauri command: re-summarize an existing note in place.
///
/// Returns the merged note body the editor should re-mount against.
/// The vault writer rotates the prior body into `<id>.md.bak` before
/// overwriting, which is what makes `heron_restore_backup` a true
/// rollback to pre-resummarize content.
#[tauri::command]
async fn heron_resummarize(vault_path: String, session_id: String) -> Result<String, String> {
    resummarize::resummarize(Path::new(&vault_path), &session_id).await
}

/// Tauri command: report whether a `<id>.md.bak` exists. `Ok(None)` when
/// no backup is on disk — the steady-state case after a save without
/// a re-summarize.
#[tauri::command]
async fn heron_check_backup(
    vault_path: String,
    session_id: String,
) -> Result<Option<resummarize::BackupInfo>, String> {
    resummarize::check_backup(Path::new(&vault_path), &session_id).await
}

/// Tauri command: restore `<id>.md` from `<id>.md.bak`. Returns the
/// restored body so the editor can re-mount immediately.
#[tauri::command]
async fn heron_restore_backup(vault_path: String, session_id: String) -> Result<String, String> {
    resummarize::restore_backup(Path::new(&vault_path), &session_id).await
}

/// Tauri command: resolve the platform-default cache directory.
///
/// The Review UI's playback bar passes this back to
/// `heron_resolve_recording` so the asset-protocol resolver can find
/// the per-session WAV mixdown when the m4a hasn't been encoded yet.
/// Mirrors [`heron_default_settings_path`]: lossy UTF-8 conversion +
/// fallback to `./` if the platform's cache dir cannot be resolved
/// (sandboxed test runners).
#[tauri::command]
fn heron_default_cache_root() -> String {
    default_cache_root().to_string_lossy().into_owned()
}

/// Phase 68 (PR-ζ): event name fired on the main webview when the
/// global hotkey triggers. The Rust handler logs + emits this; real
/// Start/Stop wiring lands in a future phase.
const EVENT_HOTKEY_FIRED: &str = "hotkey:fired";

/// Tauri command: register `combo` as the system-wide Start/Stop
/// Recording hotkey.
///
/// On success the chord is held by this app until
/// [`heron_unregister_hotkey`] runs (or the app exits). On failure the
/// returned `String` carries a human-facing reason — usually "another
/// app already owns this chord". The frontend renders the message
/// verbatim under the input.
///
/// The handler is intentionally a stub for PR-ζ: it logs `"hotkey
/// fired"` and emits the [`EVENT_HOTKEY_FIRED`] event so a future
/// recording-wiring PR can replace the body with a real FSM
/// transition without touching the registration plumbing.
#[tauri::command]
fn heron_register_hotkey(app: tauri::AppHandle, combo: String) -> Result<(), String> {
    let manager = app.global_shortcut();
    // Idempotent re-register: if the user clicks Save twice with the
    // same chord, the second `register()` would error with "already
    // registered". Treat the in-app re-register as a no-op rather than
    // surfacing an error the user can't act on.
    if manager.is_registered(combo.as_str()) {
        return Ok(());
    }
    manager
        .register(combo.as_str())
        .map_err(|e| e.to_string())?;
    // The plugin's per-shortcut handler isn't bound here — we register
    // a global handler at plugin-build time (see `run`) that fires for
    // every chord. That sidesteps the lifetime gymnastics of holding
    // an `AppHandle` inside a `'static` closure passed to
    // `on_shortcut`.
    Ok(())
}

/// Tauri command: probe whether `combo` would conflict with an
/// existing system-wide hotkey.
///
/// Returns `Ok(true)` if the chord is free (heron can register it),
/// `Ok(false)` if another app or the OS already owns it. The
/// underlying plugin's `is_registered` only reports per-app state, so
/// we attempt the OS-level register + immediately unregister to read
/// the platform's answer. This is the same pattern Electron's
/// `globalShortcut.isRegistered` uses internally on macOS.
///
/// Note: a hotkey we already own ourselves returns `true` (free),
/// since re-registering it from this app is a no-op.
#[tauri::command]
fn heron_check_hotkey(app: tauri::AppHandle, combo: String) -> Result<bool, String> {
    let manager = app.global_shortcut();
    if manager.is_registered(combo.as_str()) {
        // Already owned by us — `register()` would no-op, so the chord
        // is effectively "free for heron".
        return Ok(true);
    }
    match manager.register(combo.as_str()) {
        Ok(()) => {
            // Free — release immediately so the caller's "Save" path
            // is the canonical owner. Errors here are unexpected
            // (we just registered it) and surface as the same wire
            // error so a regression in the plugin can't go silent.
            manager
                .unregister(combo.as_str())
                .map_err(|e| e.to_string())?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// Tauri command: release a previously-registered hotkey.
///
/// Tolerant of "not registered" — the Settings pane calls this with
/// the *previous* combo before re-registering the new one, and the
/// previous combo may have failed to register at startup (e.g. user
/// changed mind without saving). Surfacing that as an error would
/// block the user from saving the new chord.
#[tauri::command]
fn heron_unregister_hotkey(app: tauri::AppHandle, combo: String) -> Result<(), String> {
    let manager = app.global_shortcut();
    if !manager.is_registered(combo.as_str()) {
        return Ok(());
    }
    manager
        .unregister(combo.as_str())
        .map_err(|e| e.to_string())
}

/// Tauri command: vault disk-usage gauge for the Audio tab.
#[tauri::command]
fn heron_disk_usage(vault_path: String) -> Result<DiskUsage, String> {
    disk_usage(Path::new(&vault_path)).map_err(|e| e.to_string())
}

/// Tauri command: purge `.wav` / `.m4a` audio sidecars older than
/// `days` days. Returns the count actually deleted.
#[tauri::command]
fn heron_purge_audio_older_than(vault_path: String, days: u32) -> Result<u32, String> {
    purge_audio_older_than(Path::new(&vault_path), days).map_err(|e| e.to_string())
}

/// Tauri command: persist the "wizard finished" flag (§13.3 / PR-ι).
///
/// Called by the desktop frontend's `Finish setup` button on the last
/// onboarding step. Reads the on-disk settings, sets `onboarded =
/// true`, writes back atomically. Idempotent — re-running on an
/// already-onboarded file is a no-op.
///
/// We resolve the path via [`default_settings_path`] (the same path
/// `heron_default_settings_path` returns to the renderer) rather than
/// accepting a renderer-supplied path, because:
///
/// 1. The "I am onboarded" flag is per-install. There is no
///    legitimate reason for the renderer to flip this flag in a
///    non-default location.
/// 2. Pinning the path here keeps the command from widening the
///    "write-anywhere" primitive surface a renderer-supplied path
///    would expose.
#[tauri::command]
fn heron_mark_onboarded() -> Result<(), String> {
    mark_onboarded(&default_settings_path()).map_err(|e| e.to_string())
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
///
/// The probe takes no arguments — it inspects
/// `HERON_WHISPERKIT_MODEL_DIR` (the same env var the orchestrator
/// reads via `WhisperKitBackend::from_env`) to answer "is a usable
/// model already on disk?". The earlier draft accepted a `progress:
/// f32` from the renderer, which inverted the trust direction (only
/// the model folder authoritatively knows whether the bundle is
/// complete) and turned the test into a pure function of its input.
#[tauri::command]
fn heron_test_model_download() -> TestOutcome {
    test_model_download()
}

/// Tauri command: navigate the frontend to the named target.
///
/// The frontend owns the router (`react-router-dom`), so the Rust side
/// can't push a route directly. Instead, this command:
///
///   1. focuses the main webview window (showing + un-minimising it
///      if the user had pushed it offscreen),
///   2. emits a `nav:<target>` event that `App.tsx`'s
///      `hooks/useTrayNav.ts` listens for and converts to a
///      `useNavigate()` call.
///
/// Returning a `Result<(), String>` lets unknown targets surface as a
/// JS rejection (caller bug) rather than silently no-op.
#[tauri::command]
fn heron_open_window(app: tauri::AppHandle, target: String) -> Result<(), String> {
    let event = tray::open_window_event_name(&target)?;
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
    app.emit(event, ()).map_err(|e| e.to_string())
}

// ---- Keychain commands (PR-θ / phase 70) --------------------------
//
// Surface for the Settings pane's API-key field. `set` and `delete`
// are user-initiated mutations; `has` and `list` are existence
// probes. There is **no** read-back command — the cleartext secret is
// never returned across the Tauri boundary, only the boolean
// "populated?" answer.
//
// All four shims use `Result<_, String>` so JS gets a plain rejection;
// `KeychainError`'s `Display` impl is safe to surface (it never embeds
// the secret value — see the type's doc comment in `keychain.rs`).

/// Parse a wire-format account label into a [`KeychainAccount`]. The
/// helper is local to the command shims so unknown labels reject with
/// a uniform message.
fn parse_account(label: &str) -> Result<KeychainAccount, String> {
    KeychainAccount::from_label(label).ok_or_else(|| format!("unknown keychain account: {label}"))
}

/// Tauri command: store `secret` for the named account in the macOS
/// login Keychain.
///
/// `secret` is consumed by the Security framework call and never logged
/// or echoed. The argument arrives over the Tauri IPC bridge — the
/// renderer is expected to obtain it from a password input field the
/// user just typed into.
///
/// Defence in depth: an empty (or whitespace-only) secret is rejected
/// here as well as in the renderer's Save button. A misbehaving
/// renderer can't write empties and turn the slot into a user-visible
/// "set" status with no real value behind it.
#[tauri::command]
fn heron_keychain_set(account: String, secret: String) -> Result<(), String> {
    let account = parse_account(&account)?;
    if secret.trim().is_empty() {
        // Note: the error message intentionally describes the value's
        // *shape* ("empty"), never any of its content.
        return Err("keychain secret must not be empty".into());
    }
    keychain::keychain_set(account, &secret).map_err(|e| e.to_string())
}

/// Tauri command: report whether the named account currently has a
/// stored entry. **Does not return the secret value** — the renderer
/// only learns "set" / "not set", which is what the UI's status pill
/// needs.
#[tauri::command]
fn heron_keychain_has(account: String) -> Result<bool, String> {
    let account = parse_account(&account)?;
    keychain::keychain_get(account)
        .map(|opt| opt.is_some())
        .map_err(|e| e.to_string())
}

/// Tauri command: delete the entry for the named account. Idempotent —
/// deleting a missing entry returns `Ok(())`.
#[tauri::command]
fn heron_keychain_delete(account: String) -> Result<(), String> {
    let account = parse_account(&account)?;
    keychain::keychain_delete(account).map_err(|e| e.to_string())
}

/// Tauri command: enumerate the wire-format labels of accounts that
/// currently have entries.
#[tauri::command]
fn heron_keychain_list() -> Result<Vec<String>, String> {
    keychain::keychain_list()
        .map(|accounts| {
            accounts
                .into_iter()
                .map(|a| a.as_str().to_owned())
                .collect()
        })
        .map_err(|e| e.to_string())
}

/// Register the user's saved hotkey at app startup so the chord works
/// from anywhere in macOS without first opening the Settings pane.
///
/// Reads `default_settings_path()` and consults the `record_hotkey`
/// field; an empty string short-circuits ("hotkey disabled"). Any
/// failure is logged and swallowed — the app should still launch even
/// if the user's saved chord conflicts with another app, since the
/// Settings pane is the user's recovery path.
fn register_startup_hotkey(app: &tauri::AppHandle) {
    let path = default_settings_path();
    let Ok(settings) = read_settings(&path) else {
        // Corrupt/missing settings is the first-run state — fall
        // through silently. The Settings pane's own load path will
        // surface the error if it persists.
        return;
    };
    if settings.record_hotkey.is_empty() {
        return;
    }
    if let Err(e) = app
        .global_shortcut()
        .register(settings.record_hotkey.as_str())
    {
        tracing::warn!(
            "could not register saved hotkey {:?}: {}",
            settings.record_hotkey,
            e
        );
    }
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

/// Default cache root.
///
/// Resolves via [`dirs::cache_dir`]:
/// - macOS: `~/Library/Caches/com.heronnote.heron`
/// - Linux: `$XDG_CACHE_HOME/com.heronnote.heron`
/// - Windows: `%LOCALAPPDATA%\com.heronnote.heron\cache`
///
/// Falls back to `./` so a sandboxed CI runner without a resolvable
/// cache dir still produces a usable path. The §15.2 asset-protocol
/// resolver expects `<cache_root>/sessions/<id>/{mic,tap}.raw`.
pub fn default_cache_root() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("com.heronnote.heron")
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
        // The Settings pane vault-path picker calls
        // `@tauri-apps/plugin-dialog::open({ directory: true })` to
        // surface the native folder picker. Registering the plugin
        // here wires up the IPC handler the JS bridge talks to.
        .plugin(tauri_plugin_dialog::init())
        // Phase 68 (PR-ζ): system-wide Start/Stop Recording hotkey.
        // The `with_handler` closure fires for *every* chord this app
        // registers — we currently only register one (the user's
        // Settings pane choice), so the handler can unconditionally
        // log + emit. Real recording wiring lands in a future phase.
        // The `Pressed` filter avoids a duplicate fire on key-release.
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        tracing::info!("hotkey fired");
                        // Best-effort emit; a missing main window
                        // (e.g. during shutdown) drops the event
                        // rather than panicking.
                        let _ = app.emit(EVENT_HOTKEY_FIRED, ());
                    }
                })
                .build(),
        )
        .setup(|app| {
            // Phase 64: install the menubar tray. The tray's polling
            // task lives on the Tauri async runtime, so it shuts down
            // cleanly when the app exits — no manual handle to track.
            //
            // We log + propagate a setup error rather than swallowing
            // it: a missing tray on macOS is a regression worth
            // surfacing in CI logs, not silently degrading.
            tray::install(app.handle())?;
            // Phase 68 (PR-ζ): register the saved hotkey at app
            // startup so the chord is live the moment the app
            // launches — not only when the user opens Settings →
            // Hotkey tab. Failures (e.g. another app already owns the
            // chord) are logged but don't block launch; the user can
            // pick a different chord in Settings without re-launching.
            register_startup_hotkey(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            heron_status,
            heron_resolve_recording,
            heron_diagnostics,
            heron_read_settings,
            heron_write_settings,
            heron_default_settings_path,
            heron_default_cache_root,
            heron_read_note,
            heron_write_note_atomic,
            heron_list_sessions,
            heron_resummarize,
            heron_check_backup,
            heron_restore_backup,
            heron_test_microphone,
            heron_test_audio_tap,
            heron_test_accessibility,
            heron_test_calendar,
            heron_test_model_download,
            heron_mark_onboarded,
            heron_open_window,
            heron_keychain_set,
            heron_keychain_has,
            heron_keychain_delete,
            heron_keychain_list,
            // Phase 69 (PR-η): crash-recovery salvage scan + per-
            // session purge surface used by `/salvage`, plus the
            // tray's "Open last note…" lookup.
            salvage::heron_scan_unfinalized,
            salvage::heron_recover_session,
            salvage::heron_purge_session,
            tray::heron_last_note_session_id,
            // Phase 68 (PR-ζ) — Settings pane Hotkey + Audio tabs.
            heron_register_hotkey,
            heron_check_hotkey,
            heron_unregister_hotkey,
            heron_disk_usage,
            heron_purge_audio_older_than,
            // Phase 73 (PR-λ) — pre-flight checks.
            heron_check_disk_for_recording,
            tray::heron_emit_capture_degraded,
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

    /// `heron_default_settings_path` exposes [`default_settings_path`]
    /// to the renderer. The string round-trip must agree with the
    /// `PathBuf` form (modulo lossy UTF-8) so the frontend can hand the
    /// returned value back into `heron_read_settings` /
    /// `heron_write_settings` without round-trip drift.
    #[test]
    fn heron_default_settings_path_matches_pathbuf_form() {
        let s = heron_default_settings_path();
        let p = default_settings_path();
        assert_eq!(s, p.to_string_lossy());
        assert!(s.ends_with("com.heronnote.heron/settings.json"));
    }

    /// Same shape as the settings-path test: the string the Tauri
    /// command exposes round-trips with [`default_cache_root`] so the
    /// Review UI's playback bar can hand the value into
    /// `heron_resolve_recording` without re-deriving it on the JS side.
    #[test]
    fn heron_default_cache_root_matches_pathbuf_form() {
        let s = heron_default_cache_root();
        let p = default_cache_root();
        assert_eq!(s, p.to_string_lossy());
        assert!(s.ends_with("com.heronnote.heron"));
    }

    /// `heron_status::audio_available` must reflect the real
    /// [`heron_audio::audio_capture_available`] probe — not the v0
    /// hardcode it replaced. The probe's return is host-dependent so
    /// we don't pin a specific bool; we pin the *equality* with the
    /// probe to guarantee a future regression that reintroduces a
    /// hardcode (or wires up a different signal) gets caught.
    ///
    /// Together with `heron_audio::audio_capture_available_is_false_off_apple`
    /// (which anchors the probe to `false` off-Apple), this transitively
    /// proves `heron_status::audio_available == false` on non-macOS —
    /// no separate off-Apple assertion needed here.
    #[test]
    fn heron_status_audio_available_matches_probe() {
        let status = heron_status();
        assert_eq!(
            status.audio_available,
            heron_audio::audio_capture_available()
        );
    }

    /// `parse_account` is the gatekeeper for the keychain command shims;
    /// any unknown label must reject before the call reaches the
    /// platform layer. A successful parse round-trips back to the same
    /// wire-format label.
    #[test]
    fn parse_account_rejects_unknown_labels() {
        assert!(parse_account("not-a-real-account").is_err());
        assert!(parse_account("").is_err());
        // Known labels round-trip via `as_str`.
        assert_eq!(
            parse_account("anthropic_api_key").map(|a| a.as_str()),
            Ok("anthropic_api_key"),
        );
        assert_eq!(
            parse_account("openai_api_key").map(|a| a.as_str()),
            Ok("openai_api_key"),
        );
    }

    /// On non-macOS targets the keychain stub returns `Unsupported`.
    /// The Tauri shim must surface that as a string error rather than
    /// panicking — exercise the full path here so a regression that
    /// e.g. unwraps the platform result gets caught at CI time.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn keychain_has_on_non_macos_surfaces_unsupported() {
        let res = heron_keychain_has("anthropic_api_key".into());
        assert!(res.is_err(), "expected Unsupported error on non-macOS");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn keychain_list_on_non_macos_surfaces_unsupported() {
        let res = heron_keychain_list();
        assert!(res.is_err(), "expected Unsupported error on non-macOS");
    }

    /// Empty-secret rejection is a defence-in-depth check: the
    /// renderer's Save button already disables on empty input, but a
    /// misbehaving / compromised renderer must not be able to write an
    /// empty value that the UI then renders as "key set". The check
    /// runs *before* the platform call, so this assertion holds on
    /// every target — no `cfg` gate needed.
    #[test]
    fn keychain_set_rejects_empty_secret() {
        let res = heron_keychain_set("anthropic_api_key".into(), String::new());
        assert!(res.is_err());
        let res2 = heron_keychain_set("anthropic_api_key".into(), "   ".into());
        assert!(res2.is_err(), "whitespace-only secret must reject too");
    }
}
