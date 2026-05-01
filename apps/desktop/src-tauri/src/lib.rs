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

pub mod action_items;
pub mod asset_protocol;
pub mod asset_scope;
pub mod daemon;
pub mod diagnostics;
pub mod disk;
pub mod event_bus;
pub mod events_bridge;
pub mod frontend_error;
pub mod keychain;
pub mod keychain_resolver;
pub mod meetings;
pub mod model_download;
pub mod notes;
pub mod onboarding;
pub mod preflight;
pub mod resummarize;
pub mod runtime_checks;
pub mod salvage;
pub mod settings;
pub mod shortcuts;
pub mod tray;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use heron_orchestrator::Builder as OrchestratorBuilder;
// Issue #206: now used directly by `build_orchestrator_for_settings`,
// which both the boot hook and the `heron_write_settings` rebuild
// path call to mint a fresh orchestrator from `Settings`.
use heron_orchestrator::LocalSessionOrchestrator;
use serde::Serialize;
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

pub use asset_protocol::{AssetError, AssetSource, resolve_recording_uri};
pub use daemon::{DaemonHandle, DaemonStatus};
pub use diagnostics::{DiagnosticsError, DiagnosticsView, SessionLog, read_diagnostics};
pub use disk::{
    DiskError, DiskUsage, disk_usage, purge_audio_older_than, purge_summaries_older_than,
};
pub use frontend_error::{ErrorClass, FRONTEND_ERRORS_TOTAL, FrontendErrorReport};
pub use onboarding::{
    TestOutcome, test_accessibility, test_accessibility_async, test_audio_tap,
    test_audio_tap_async, test_calendar, test_calendar_async, test_daemon, test_daemon_async,
    test_microphone, test_microphone_async, test_model_download,
};
pub use preflight::{DiskCheckOutcome, check_disk, heron_check_disk_for_recording};
pub use runtime_checks::{
    RuntimeCheckEntry, Severity as RuntimeCheckSeverity, heron_run_runtime_checks,
};

// Tauri's command-handler macro requires the function names it
// generates wrappers for to live at the same path the macro is in;
// we keep the underlying free functions in `onboarding` for direct
// unit-testing, and the `#[tauri::command]` shims below thread the
// arguments through.
pub use keychain::{KEYCHAIN_SERVICE, KeychainAccount, KeychainError};
pub use keychain_resolver::EnvThenKeychainResolver;
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
///
/// Per issue #197, also extends the runtime asset-protocol scope to
/// cover the new `vault_root` so the playback bar can keep playing
/// archival m4a files via `convertFileSrc` after the user moves their
/// vault. Scope is additive — a user who switches vaults back and forth
/// retains read access to both for the lifetime of the app process.
///
/// Issue #206: when `Settings.vault_root` changed compared to the
/// previously-persisted value, also tear down and rebuild the
/// in-process daemon's [`LocalSessionOrchestrator`] so subsequent
/// captures land in the new vault and `heron_list_meetings` reads
/// the new path. Rebuild is serialized behind [`RebuildSlot`] so two
/// concurrent settings saves can't race the orchestrator swap. The
/// `extend_for_vault` call composes with rebuild — the new path is
/// allowed in the asset-protocol scope before any resolver lookup
/// against it can fire from a freshly-spawned daemon task.
///
/// Wire shape preserved (issue #201's IPC contract test): same
/// `settings_path: String, settings: Settings` request body, same
/// `Result<(), String>` reply. `async fn` is transparent across the
/// Tauri IPC bridge.
#[tauri::command]
async fn heron_write_settings(
    app: tauri::AppHandle,
    settings_path: String,
    settings: Settings,
) -> Result<(), String> {
    let slot = app
        .try_state::<RebuildSlot>()
        .ok_or_else(|| "rebuild slot not installed (programming bug)".to_string())?;
    // Hold the slot's mutex across the entire write+rebuild
    // sequence. This serializes concurrent saves end-to-end, so
    // two requests writing different vaults can't interleave their
    // disk writes vs. their daemon rebinds — the last writer to
    // acquire the lock is also the last to rebind, which keeps the
    // on-disk `vault_root` and the daemon's `AppState` consistent.
    // Pre-fix the lock was held only across the rebuild segment;
    // Codex review caught the resulting race where save B's write
    // could win on disk while save A's rebuild won at the daemon.
    let mut current_guard = slot.0.lock().await;

    let settings_path_buf = PathBuf::from(&settings_path);
    let next_vault_trimmed = settings.vault_root.trim().to_owned();
    // Compare against the slot's last-applied vault root rather
    // than re-reading the disk: the slot reflects what the daemon
    // is *actually* serving against, which is the right source of
    // truth for "do we need to rebuild?" — disk could be ahead or
    // behind if a previous save's rebuild failed mid-flight.
    let vault_changed = current_guard.applied_vault_root != next_vault_trimmed;

    write_settings(&settings_path_buf, &settings).map_err(|e| e.to_string())?;
    asset_scope::extend_for_vault(&app, &settings.vault_root);

    if vault_changed {
        rebuild_orchestrator_on_vault_change(&app, &settings, &mut current_guard).await?;
    }
    Ok(())
}

/// Issue #206: build a fresh [`LocalSessionOrchestrator`] from the
/// supplied settings, applying the same vault-root precedence the
/// boot path uses (configured non-empty `Settings.vault_root` wins,
/// else fall back to [`resolve_vault_root`]).
///
/// Pulled out so the boot hook and the `heron_write_settings` rebuild
/// path go through one function — a future Settings field that needs
/// to flow into the orchestrator (next hotwords-style addition) only
/// has to be plumbed here once. Returns a fresh `Arc` so callers can
/// pass identical clones to the daemon's [`AppState`] and the
/// orchestrator-shutdown bookkeeping.
///
/// # Panics
///
/// Same Tokio-runtime requirement as
/// [`heron_orchestrator::Builder::build`] — must be called from
/// inside a Tokio runtime context. The desktop's setup hook satisfies
/// this via `tauri::async_runtime::block_on`; the
/// `heron_write_settings` rebuild path satisfies it because Tauri
/// `async fn` commands run on the Tauri-managed runtime.
fn build_orchestrator_for_settings(settings: &Settings) -> Arc<LocalSessionOrchestrator> {
    let vault_root = match settings.vault_root.trim() {
        "" => resolve_vault_root(),
        s => Some(PathBuf::from(s)),
    };
    let file_naming_pattern: heron_vault::FileNamingPattern = settings.file_naming_pattern.into();
    let mut builder = OrchestratorBuilder::default()
        .hotwords(settings.hotwords.clone())
        .file_naming_pattern(file_naming_pattern)
        .auto_detect_meeting_app(settings.auto_detect_meeting_app);
    if let Some(root) = vault_root {
        builder = builder.vault_root(root);
    }
    Arc::new(builder.build())
}

/// Issue #206: serialization lock + current-orchestrator slot for the
/// [`heron_write_settings`] rebuild path. Stored as managed Tauri
/// state so concurrent settings saves (e.g. the renderer's debounced
/// auto-save firing while the user clicks Save in another tab)
/// serialize the orchestrator swap instead of racing for the daemon's
/// port.
///
/// The same `tokio::sync::Mutex` guards both pieces of state so the
/// "lock and swap" sequence is one atomic critical section: the
/// rebuild path can read the previous orchestrator, install the new
/// one, and rebind the daemon without a sibling save observing the
/// half-swapped state. `tokio::sync::Mutex` rather than
/// `std::sync::Mutex` because the path holds the lock across a
/// `.await` (axum drain + orchestrator shutdown + new bind).
///
/// The wrapped `Arc<LocalSessionOrchestrator>` is the orchestrator
/// the daemon's [`AppState`] was last bound to. Boot installs it
/// once; the rebuild path swaps it in place and `shutdown().await`s
/// the previous value before binding the new daemon — the
/// deterministic teardown the constraint in CLAUDE.md and the
/// orchestrator's `shutdown_tx` field doc both call out.
///
/// `applied_vault_root` is the trimmed `Settings.vault_root` that
/// produced the current orchestrator. Re-checking the renderer-
/// supplied value against this *after* the lock is acquired
/// suppresses the redundant-rebuild race two concurrent
/// `heron_write_settings` calls would otherwise trip: both observe
/// the same on-disk previous value, both queue a rebuild, the
/// second one would otherwise rebuild a second time against the
/// already-up-to-date orchestrator.
pub(crate) struct RebuildSlot(pub(crate) tokio::sync::Mutex<RebuildSlotInner>);

pub(crate) struct RebuildSlotInner {
    pub(crate) orchestrator: Arc<LocalSessionOrchestrator>,
    pub(crate) applied_vault_root: String,
    /// Tier 5 #26 auto-record scheduler handle for the *current*
    /// orchestrator. Tracked so the rebuild path can abort the
    /// previous scheduler explicitly — the `event_bus::install_with`
    /// path managed an `Arc<LocalSessionOrchestrator>` at boot that
    /// keeps the previous orchestrator alive past the slot swap, so
    /// the scheduler's `Weak::upgrade` would otherwise keep
    /// returning `Some` and the old scheduler would tick against
    /// the old vault concurrently with the new one. Codex review
    /// caught this race.
    pub(crate) auto_record_scheduler: tokio::task::JoinHandle<()>,
}

/// Issue #206: rebuild the in-process orchestrator and re-bind the
/// daemon onto it. Called by `heron_write_settings` only when
/// `Settings.vault_root` changed.
///
/// Lifecycle ordering (matters for correctness):
///
/// 1. Acquire the [`RebuildSlot`] lock. Concurrent settings saves
///    serialize here so the daemon never sees a half-swapped state.
/// 2. Build the new orchestrator with
///    [`build_orchestrator_for_settings`] *before* tearing down the
///    old one, so a build failure (e.g. auto-record-registry I/O
///    panic) leaves the daemon serving the previous orchestrator
///    unchanged.
/// 3. [`daemon::shutdown_for_rebuild`] signals the old axum task's
///    shutdown and awaits its drain (with timeout). axum is no
///    longer accepting requests when this returns.
/// 4. Call [`LocalSessionOrchestrator::shutdown`] on the previous
///    orchestrator. This is the deterministic-teardown path the
///    orchestrator's `shutdown_tx` field doc calls out: the
///    recorder task exits cleanly here, before the new daemon
///    starts serving requests against the new orchestrator. Errors
///    are logged but proceed — `Drop` fires the same signal as a
///    fallback per the orchestrator's docs.
/// 5. [`daemon::bind_after_rebuild`] re-binds 7384 and spawns a
///    fresh axum task against the new orchestrator. The daemon's
///    `AppState` now points at the new vault.
/// 6. Replace the slot's `Arc<LocalSessionOrchestrator>` and
///    `applied_vault_root` with the new values, and abort the
///    previous auto-record scheduler. Atomic with respect to
///    step 4 because we hold the slot's mutex throughout — and
///    only reached when the bind in step 5 succeeded.
///
/// Bind ordering (step 5 before step 6) is intentional: a bind
/// failure must NOT update `applied_vault_root`, otherwise a
/// retry of the same vault would observe a stale "already
/// applied" state and skip the rebuild. Codex review caught the
/// pre-fix bug where the slot was committed before the bind ran.
///
/// On any failure path between steps 3 and 5 the daemon ends up
/// without a serving task — the user's settings save returns an
/// error, the renderer surfaces it, and a retry with the same
/// vault re-enters rebuild (slot's `applied_vault_root` is still
/// the *previous* value because step 6 didn't run). On-disk
/// settings still reflect the user's choice (written before this
/// function fires), so a relaunch is a clean recovery too.
async fn rebuild_orchestrator_on_vault_change(
    app: &tauri::AppHandle,
    settings: &Settings,
    current_guard: &mut tokio::sync::MutexGuard<'_, RebuildSlotInner>,
) -> Result<(), String> {
    let next_vault_trimmed = settings.vault_root.trim();
    let new_orchestrator = build_orchestrator_for_settings(settings);
    // Tier 5 #26 parity: the boot path calls
    // `spawn_auto_record_scheduler` on the freshly-built orchestrator
    // so per-event auto-record fires off the calendar reader. The
    // post-rebuild orchestrator needs the same scheduler running
    // against the new vault — without it, a user who switches vaults
    // mid-day would silently lose auto-record until restart. We
    // capture the new scheduler's handle so it lives in the slot;
    // the *previous* scheduler is aborted at step 6 once the new
    // orchestrator is committed, so two schedulers can't tick
    // concurrently against two vaults.
    let new_auto_record_scheduler = new_orchestrator.spawn_auto_record_scheduler();

    // Step 3: tear down the old daemon's axum task and await drain.
    daemon::shutdown_for_rebuild(app)
        .await
        .map_err(|e| e.to_string())?;

    // Step 4: deterministic recorder teardown on the previous
    // orchestrator. Cloning the Arc lets us call `shutdown` (which
    // takes `&self` but consumes internal `oneshot::Sender`s under
    // its own Mutex) without dropping the slot's reference yet —
    // the swap in step 5 is what releases the previous Arc.
    let previous = Arc::clone(&current_guard.orchestrator);
    if let Err(e) = previous.shutdown().await {
        // The orchestrator's `Drop` impl fires the same signal
        // best-effort, so a join error here just means the recorder
        // task panicked or was cancelled — the new orchestrator's
        // recorder is independent and unaffected.
        tracing::warn!(
            error = %e,
            "previous orchestrator shutdown errored; recorder task may exit via Drop fallback",
        );
    }
    // Drop our extra clone before the swap so the slot holds the
    // sole boot-time reference to the old orchestrator (about to
    // be replaced).
    drop(previous);

    // Step 5: bind a fresh axum task against the new orchestrator.
    // We do this *before* committing the slot swap so that a bind
    // failure doesn't desync the slot from the actual daemon state
    // (Codex review caught the bug: a bind failure after slot
    // commit would update `applied_vault_root` to a vault the
    // daemon never bound, and a retry with the same vault would be
    // skipped as a no-op).
    if let Err(e) = daemon::bind_after_rebuild(app, Arc::clone(&new_orchestrator)).await {
        // The new orchestrator's scheduler is running but the
        // daemon never bound to it — abort the scheduler so it
        // doesn't tick against a vault no daemon serves. The
        // recorder shutdown is best-effort via Drop on the
        // upcoming `new_orchestrator` drop.
        new_auto_record_scheduler.abort();
        return Err(e.to_string());
    }

    // Step 6: install the new orchestrator + scheduler handle into
    // the slot atomically with `applied_vault_root` so the next
    // save sees a coherent (orchestrator, vault_root, scheduler)
    // tuple. Only reached when the bind succeeded — a failed bind
    // exits via `?` above and leaves the slot pointing at the
    // previous orchestrator's `Arc`, which is now in the
    // "shutdown-but-not-rebound" state. A subsequent retry of the
    // same vault re-enters with `vault_changed = true` (slot's
    // `applied_vault_root` is still the old value) and attempts
    // the rebind again — the recovery path the user expects from
    // a transient bind failure.
    //
    // Abort the previous scheduler before swapping the handle:
    // the `event_bus::install_with` path managed an `Arc` that
    // keeps the previous orchestrator alive past the slot swap,
    // so the previous scheduler's `Weak::upgrade` would still
    // succeed and the old vault would receive auto-record fires
    // concurrent with the new one.
    let old_scheduler = std::mem::replace(
        &mut current_guard.auto_record_scheduler,
        new_auto_record_scheduler,
    );
    old_scheduler.abort();
    current_guard.orchestrator = new_orchestrator;
    current_guard.applied_vault_root = next_vault_trimmed.to_owned();

    tracing::info!(
        vault_root = %settings.vault_root,
        "in-process orchestrator rebuilt after vault_root change (issue #206)",
    );
    Ok(())
}

/// Tauri command: read `<vault>/meetings/<basename>.md`, where
/// `<basename>` is `session_id` with any `mtg_` wire prefix stripped
/// to match `heron_vault::VaultWriter`'s on-disk shape.
///
/// `vault_path` and `session_id` are passed as separate strings so the
/// path policy enforced by `notes::read_note` (validate basename,
/// canonicalize, reject vault escapes) cannot be bypassed by
/// constructing the joined path on the renderer side.
#[tauri::command]
async fn heron_read_note(vault_path: String, session_id: String) -> Result<String, String> {
    notes::read_note(Path::new(&vault_path), &session_id).await
}

/// Tauri command: atomic-write `<vault>/meetings/<basename>.md` (editor blur / ⌘S).
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

/// Tauri command: PR-ξ (phase 76) preview the post-merge note for the
/// diff modal. Runs the same summarize + §10.3 merge pipeline as
/// [`heron_resummarize`] but never writes `<id>.md` and never rotates
/// `<id>.md.bak` — the renderer compares the returned string against
/// `heron_read_note` and the user clicks Apply (which fires
/// [`heron_resummarize`]) or Cancel.
#[tauri::command]
async fn heron_resummarize_preview(
    vault_path: String,
    session_id: String,
) -> Result<String, String> {
    resummarize::resummarize_preview(Path::new(&vault_path), &session_id).await
}

/// Tauri command (Day 8-10 #19): apply a per-row patch against a single
/// action item in `<vault>/meetings/<basename>.md`'s frontmatter
/// (`<basename>` strips the `mtg_` wire prefix). Returns the post-merge
/// row so the renderer can drop optimistic UI without a follow-up
/// `heron_get_meeting`.
///
/// `meetingId` is the wire-form id (or on-disk basename) — same ID
/// `heron_resummarize` consumes — and is named `meetingId` on the
/// JS-side wire to align with the daemon's `MeetingId` semantics
/// (today the desktop's read path uses these as interchangeable
/// pointers to the same meeting note). `itemId` is the
/// `Frontmatter.action_items[].id` UUID minted by the vault writer.
///
/// Patch semantics follow JSON Merge Patch (RFC 7396). See
/// [`action_items::update_action_item`] for the per-field rules and
/// [`heron_vault::ActionItemPatch`] for the wire shape.
#[tauri::command]
async fn heron_update_action_item(
    vault_path: String,
    meeting_id: String,
    item_id: String,
    patch: heron_vault::ActionItemPatch,
) -> Result<action_items::ActionItemView, String> {
    action_items::update_action_item(Path::new(&vault_path), &meeting_id, &item_id, patch).await
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
/// global hotkey triggers. Tier 4 #24 emits this only for the canonical
/// [`shortcuts::ACTION_TOGGLE_RECORDING`] action id (see
/// [`shortcuts::emit_for_action`]) — pre–Tier-4 listeners that toggle
/// recording on `hotkey:fired` continue to work unchanged, while new
/// action ids (e.g. `summarize_now`) emit only their per-action
/// `shortcut:<id>` event.
pub(crate) const EVENT_HOTKEY_FIRED: &str = "hotkey:fired";

/// Tauri command: register `combo` as the system-wide Start/Stop
/// Recording hotkey.
///
/// On success the chord is held by this app until
/// [`heron_unregister_hotkey`] runs (or the app exits). On failure the
/// returned `String` carries a human-facing reason — usually "another
/// app already owns this chord". The frontend renders the message
/// verbatim under the input.
///
/// Tier 4 #24: routes through `on_shortcut` with the canonical
/// [`shortcuts::ACTION_TOGGLE_RECORDING`] action id so the same
/// `shortcut:toggle_recording` + legacy `hotkey:fired` events fire
/// regardless of whether the chord was registered at startup or from
/// the Settings pane's "Save" button.
#[tauri::command]
fn heron_register_hotkey(app: tauri::AppHandle, combo: String) -> Result<(), String> {
    let manager = app.global_shortcut();
    // Idempotent re-register: if the user clicks Save twice with the
    // same chord, the second `on_shortcut` would error with "already
    // registered". Treat the in-app re-register as a no-op rather than
    // surfacing an error the user can't act on.
    if manager.is_registered(combo.as_str()) {
        return Ok(());
    }
    manager
        .on_shortcut(combo.as_str(), |app, _shortcut, event| {
            if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                shortcuts::emit_for_action_public(app, shortcuts::ACTION_TOGGLE_RECORDING);
            }
        })
        .map_err(|e| e.to_string())
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

/// Tauri command (Tier 4 #24): drain and return any
/// [`shortcuts::ConflictNotice`]s captured during startup
/// registration.
///
/// The frontend calls this once on mount to surface a one-shot Sonner
/// toast for each conflict the user introduced by hand-editing
/// `settings.json`. Pairs with the [`shortcuts::EVENT_CONFLICT`] event
/// (live conflicts after launch); together they cover both the
/// "webview wasn't listening yet" startup case and the eventual
/// hot-reload path.
#[tauri::command]
fn heron_take_pending_shortcut_conflicts(
    state: tauri::State<'_, shortcuts::PendingConflicts>,
) -> Vec<shortcuts::ConflictNotice> {
    state.drain()
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

/// Tauri command (issue #226): record a frontend render-time error.
///
/// Called by the renderer's `ErrorBoundary` (and by the
/// `unhandledrejection` handler) when a React subtree throws. The
/// renderer constructs a redacted [`FrontendErrorReport`] from explicit
/// safe fields — never `JSON.stringify(props)` — and fires this
/// command without awaiting; see `apps/desktop/src/lib/errorReport.ts`.
///
/// The handler:
///   1. Bumps the `frontend_errors_total{component, error_class}`
///      Prometheus counter on the same recorder
///      [`heron_metrics::init_prometheus_recorder`] installs at daemon
///      startup. The component label flows through
///      [`heron_metrics::RedactedLabel::hashed`] for cardinality
///      safety; `error_class` is a closed enum with snake_case
///      discriminants.
///   2. Logs the structured payload via `tracing::warn!` so the full
///      report (message + stack + route) lands in the daemon's normal
///      log stream and the diagnostics bundle.
///
/// Returns `Result<(), String>` for parity with the other commands;
/// every code path resolves `Ok(())`. The renderer treats this as
/// fire-and-forget — the ErrorBoundary UI must not block on the IPC
/// (the daemon may be down).
#[tauri::command]
fn heron_report_frontend_error(report: FrontendErrorReport) -> Result<(), String> {
    frontend_error::report_frontend_error(report);
    Ok(())
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

/// Tauri command: Tier 4 sibling of [`heron_purge_audio_older_than`].
/// Purges `.md` summary files at the vault root whose mtime is older
/// than `days` days. Returns the count actually deleted. Driven by
/// `Settings.summary_retention_days`; the audio sidecars are never
/// candidates (see `purge_summaries_keeps_audio_deletes_old_md`).
#[tauri::command]
fn heron_purge_summaries_older_than(vault_path: String, days: u32) -> Result<u32, String> {
    purge_summaries_older_than(Path::new(&vault_path), days).map_err(|e| e.to_string())
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

/// Tauri command: §13.3 step 6 (Gap #7) — daemon liveness Test button.
///
/// Probes the in-process / loopback `herond` at `/v1/health`. Returns
/// [`TestOutcome::Pass`] on a 200 with a parseable body,
/// [`TestOutcome::Fail`] otherwise. The wizard's React side renders
/// the same `TestOutcome` shape it already does for the other five
/// steps — see the comment in `onboarding.rs::test_daemon` for the
/// JS-side wiring expectation.
#[tauri::command]
async fn heron_test_daemon() -> TestOutcome {
    test_daemon_async().await
}

/// Tauri command: surface the in-process daemon status for any UI
/// surface that wants to render "daemon up?" without going through
/// the onboarding [`TestOutcome`] shape (the menubar tray, a future
/// status pill in the toolbar, etc.). Returns the structured
/// [`DaemonStatus`] so the frontend can distinguish "running" /
/// "version" / "error" without parsing a single string.
#[tauri::command]
async fn heron_daemon_status() -> DaemonStatus {
    daemon::probe().await
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

/// Tauri command (gap #5b): trigger the real WhisperKit model download.
///
/// Replaces the prior step-5 placeholder badge that only checked
/// whether a model was already on disk. Forwards 0..1 progress ticks
/// onto the `model_download:progress` Tauri event the renderer
/// listens on. See [`crate::model_download`] for the wire shape and
/// the per-error mapping.
#[tauri::command]
async fn heron_download_model(app: tauri::AppHandle) -> Result<String, String> {
    model_download::run_download(app).await
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

/// Tauri command: reveal the user's vault folder in Finder.
///
/// Reads `vault_root` from the on-disk settings file rather than
/// trusting the renderer — a compromised webview cannot redirect
/// `open(1)` at an arbitrary directory. The path is canonicalized
/// before launch so option-shaped names cannot be parsed as flags.
#[tauri::command]
async fn heron_open_vault_folder(settings_path: String) -> Result<(), String> {
    let settings = read_settings(Path::new(&settings_path)).map_err(|e| e.to_string())?;
    let vault_root = settings.vault_root.trim();
    if vault_root.is_empty() {
        return Err("vault folder is not configured".to_string());
    }
    let path = Path::new(vault_root);
    if !path.exists() {
        return Err(format!("vault folder not found: {vault_root}"));
    }
    if !path.is_dir() {
        return Err(format!("vault path is not a directory: {vault_root}"));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("failed to canonicalize vault folder: {e}"))?;
    open_vault_in_finder(&canonical).await
}

#[cfg(target_os = "macos")]
async fn open_vault_in_finder(path: &Path) -> Result<(), String> {
    let status = tokio::process::Command::new("open")
        .arg("--")
        .arg(path)
        .status()
        .await
        .map_err(|e| format!("failed to open vault folder: {e}"))?;
    if !status.success() {
        return Err(format!("`open` exited with status {status}"));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn open_vault_in_finder(_path: &Path) -> Result<(), String> {
    Err("opening the vault folder is only supported on macOS in v1".to_string())
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
///
/// Gap #2 (this PR): on success, also mirror the new value into the
/// process env via [`keychain::sync_env_for_account`] so the in-process
/// `herond` daemon — and any subprocess it spawns (the Codex / Claude
/// Code summarizer backends, eventually `OpenAiRealtime` once the
/// orchestrator wires it) — picks up the edit without an app restart.
/// The env-mirror step runs only after the keychain write succeeded,
/// so a backend failure leaves both surfaces consistent ("nothing
/// changed").
#[tauri::command]
fn heron_keychain_set(account: String, secret: String) -> Result<(), String> {
    let account = parse_account(&account)?;
    if secret.trim().is_empty() {
        // Note: the error message intentionally describes the value's
        // *shape* ("empty"), never any of its content.
        return Err("keychain secret must not be empty".into());
    }
    keychain::keychain_set(account, &secret).map_err(|e| e.to_string())?;
    // Mirror the same value into the process env so the daemon's
    // `OPENAI_API_KEY` lookup succeeds without an app restart. We pass
    // exactly what was written to the keychain (no extra trim) so the
    // two surfaces stay byte-identical.
    keychain::sync_env_for_account(account, Some(secret.as_str()));
    Ok(())
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
///
/// Gap #2 (this PR): mirror the deletion into the process env via
/// [`keychain::sync_env_for_account`]. We only touch the env after the
/// keychain delete succeeded, so a backend failure leaves the env
/// alone (the orchestrator will continue to use the value the daemon
/// hydrated at startup). Idempotency is preserved: a delete of a
/// missing entry is `Ok(())` from the keychain, and clearing an
/// already-unset env var is a no-op.
#[tauri::command]
fn heron_keychain_delete(account: String) -> Result<(), String> {
    let account = parse_account(&account)?;
    keychain::keychain_delete(account).map_err(|e| e.to_string())?;
    keychain::sync_env_for_account(account, None);
    Ok(())
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

/// Register the user's saved hotkeys at app startup so the chords work
/// from anywhere in macOS without first opening the Settings pane.
///
/// Tier 4 #24: iterates [`Settings::shortcuts`] (an action_id → accel
/// map) and registers each entry via `tauri-plugin-global-shortcut`,
/// emitting `shortcut:<action_id>` to the renderer on each firing.
/// [`Settings::record_hotkey`] is preserved as the default for the
/// canonical [`shortcuts::ACTION_TOGGLE_RECORDING`] action id; an
/// explicit `shortcuts.toggle_recording` entry overrides it. See
/// [`crate::shortcuts`] for the full merge / conflict / invalid-accel
/// contract.
///
/// Any failure is logged and swallowed — the app should still launch
/// even if a saved chord conflicts with another app, since the
/// Settings pane is the user's recovery path.
fn register_startup_hotkey(app: &tauri::AppHandle) {
    let path = default_settings_path();
    let Ok(settings) = read_settings(&path) else {
        // Corrupt/missing settings is the first-run state — fall
        // through silently. The Settings pane's own load path will
        // surface the error if it persists.
        return;
    };
    let _ = shortcuts::register_all(app, &settings.record_hotkey, &settings.shortcuts);
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

/// Resolve the vault root the in-process [`LocalSessionOrchestrator`]
/// scans for `<vault>/meetings/*.md`. Mirrors the precedence the
/// standalone `herond` binary uses (`crates/herond/src/main.rs::resolve_vault_root`):
///
/// 1. `HERON_VAULT_ROOT` env var (trimmed; an empty / whitespace-only
///    value is treated as unset, otherwise `PathBuf::from("")` would
///    silently resolve to the CWD).
/// 2. `~/heron-vault` default.
///
/// Returns `None` when the home directory itself is unresolvable
/// (sandboxed test runners). The caller treats `None` as "no vault"
/// — the orchestrator's read methods will then return
/// `NotYetImplemented` for read endpoints, which matches the daemon's
/// behaviour on a fresh install before any meetings exist.
///
/// Pulled out of the setup hook so the same precedence rule is one
/// well-tested call rather than reimplemented inline. We deliberately
/// don't `mkdir -p` the path here: the vault writer creates it at
/// first capture, and an absent vault is reported as a down vault
/// component on `/health`.
fn resolve_vault_root() -> Option<PathBuf> {
    if let Ok(s) = std::env::var("HERON_VAULT_ROOT") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|h| h.join("heron-vault"))
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
        // Phase 68 (PR-ζ) + Tier 4 #24: system-wide global shortcut
        // plugin. The previous incarnation registered a `with_handler`
        // that fired `hotkey:fired` for *every* chord — fine when only
        // one was ever registered, but a regression once Tier 4 lets
        // users bind multiple action ids (e.g. `summarize_now`), since
        // every chord would falsely toggle recording on pre–Tier-4
        // listeners. Per-shortcut handlers are now installed by
        // `shortcuts::register_all` (called from
        // `register_startup_hotkey` and the `record_hotkey` Tauri
        // command), and `shortcuts::emit_for_action` re-emits
        // `hotkey:fired` only for [`shortcuts::ACTION_TOGGLE_RECORDING`].
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        // Phase 75 (PR-ν): native notification surface used by the
        // tray's "Open last note…" no-notes-yet fallback. Registered
        // unconditionally — on first use macOS prompts the user for
        // notification permission, and our caller treats a denial as
        // a silent no-op (see `tray::notify_no_last_note`) so a user
        // who declines the prompt still gets the focus-the-window
        // affordance from the tray click without any error.
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // Gap #2 (this PR): bridge the macOS login Keychain into the
            // process env *before* anything that might read
            // `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` runs. The
            // in-process daemon constructed below — and the
            // `OpenAiRealtime` backend the orchestrator will eventually
            // build from it — both consume those env vars via
            // `std::env::var`. Hydrating here means the user can
            // configure their key entirely from Settings → Summarizer
            // and never need to export anything in their shell. Env
            // already-set wins; an empty / unset env falls through to
            // the keychain. See `keychain::hydrate_env_from_keychain`
            // for the full precedence + safety contract.
            //
            // The hydration result is logged via `tracing` (count of
            // slots populated, never the secret content). A failure
            // here is non-fatal — the function logs warnings for any
            // backend errors and returns the count it did manage to
            // populate. The daemon will surface a clearer "missing
            // key" error if a meeting tries to start without one.
            let hydrated = keychain::hydrate_env_from_keychain();
            tracing::info!(
                hydrated_count = hydrated,
                "keychain hydration: environment ready for in-process daemon",
            );

            // Phase 64: install the menubar tray. The tray's polling
            // task lives on the Tauri async runtime, so it shuts down
            // cleanly when the app exits — no manual handle to track.
            //
            // We log + propagate a setup error rather than swallowing
            // it: a missing tray on macOS is a regression worth
            // surfacing in CI logs, not silently degrading.
            tray::install(app.handle())?;
            // Gap #7 (this PR) + phase 82: build a single shared
            // `LocalSessionOrchestrator` and hand the same `Arc` to
            // both:
            //   - `event_bus::install_with` — the Tauri-IPC fan-out
            //     forwarder, so a future in-process publisher
            //     reaches the WebView, AND
            //   - `daemon::install` — the in-process axum service
            //     that mirrors the standalone `herond` binary,
            //     so HTTP/SSE consumers (CLI, future external API)
            //     see the same bus + replay cache.
            //
            // Construction must run inside the Tauri-managed Tokio
            // runtime because `LocalSessionOrchestrator::new`
            // `tokio::spawn`s its recorder task. The setup hook runs
            // on Tauri's main thread without that thread-local, so
            // we wrap in `tauri::async_runtime::block_on`. The
            // daemon's `bind()` is also async and runs in the same
            // block_on so a bind error is observable here (logged +
            // soft-failed inside `daemon::install`).
            let app_handle = app.handle().clone();
            // Tier 4 #17 / #19 / #23: read boot settings once and seed
            // hotwords, file naming, and auto-detect into the
            // orchestrator. A corrupt / missing `settings.json` is the
            // first-run state — fall back to defaults rather than
            // failing setup, mirroring `register_startup_hotkey`.
            //
            // Issue #206: the same boot settings flow into
            // `build_orchestrator_for_settings`, which the
            // `heron_write_settings` rebuild path also uses — so the
            // boot orchestrator and a post-vault-swap rebuilt
            // orchestrator are configured identically.
            let boot_settings = read_settings(&default_settings_path()).unwrap_or_default();
            // Issue #197: tighten the asset-protocol scope from `["**"]`
            // to exactly the directories the playback bar reads. Cache
            // covers `<cache>/sessions/<id>/{mic,tap}.raw` (salvage) and
            // `<cache>/daemon-audio/<id>.m4a` (daemon fetch); vault
            // covers `<vault>/meetings/<basename>.m4a` (archival). The
            // `heron_write_settings` command extends scope when the user
            // moves their vault from Settings.
            //
            // Same vault-root precedence as `build_orchestrator_for_settings`:
            // configured non-empty wins, else fall back to
            // `resolve_vault_root`. Computed inline here because the
            // orchestrator builder consumes the value but
            // `install_initial_scope` only borrows.
            let boot_vault_root = match boot_settings.vault_root.trim() {
                "" => resolve_vault_root(),
                s => Some(PathBuf::from(s)),
            };
            asset_scope::install_initial_scope(
                app.handle(),
                &default_cache_root(),
                boot_vault_root.as_deref(),
            );
            tauri::async_runtime::block_on(async move {
                let orchestrator = build_orchestrator_for_settings(&boot_settings);
                if let Some(ref root) = boot_vault_root {
                    tracing::info!(
                        vault_root = %root.display(),
                        ?boot_settings.file_naming_pattern,
                        auto_detect_meeting_app = boot_settings.auto_detect_meeting_app,
                        "in-process orchestrator: read-side wired against vault",
                    );
                } else {
                    // Sandboxed test runner / no home dir.
                    // Substrate-only — every read endpoint will
                    // return NotYetImplemented, which is the
                    // honest answer until a vault is configured.
                    tracing::warn!(
                        auto_detect_meeting_app = boot_settings.auto_detect_meeting_app,
                        "no vault root resolvable; in-process orchestrator runs substrate-only",
                    );
                }
                let auto_record_scheduler = orchestrator.spawn_auto_record_scheduler();
                event_bus::install_with(&app_handle, Arc::clone(&orchestrator))?;
                daemon::install(&app_handle, Arc::clone(&orchestrator)).await?;
                // Issue #206: stash the boot orchestrator under the
                // rebuild slot so the `heron_write_settings` rebuild
                // path can find it later, call `shutdown` on it
                // (deterministic recorder teardown), and swap a fresh
                // orchestrator into the slot atomically with the
                // daemon rebind. The same managed state doubles as the
                // rebuild lock — concurrent saves serialize on its
                // `tokio::sync::Mutex`. Seed `applied_vault_root` with
                // the trimmed boot value so the redundant-rebuild
                // suppression in `rebuild_orchestrator_on_vault_change`
                // recognizes a no-op save against the same vault.
                // The auto-record scheduler handle is parked in the
                // slot so the rebuild path can `abort()` it before
                // spawning the new orchestrator's scheduler — the
                // `event_bus::install_with` path managed an Arc that
                // keeps the previous orchestrator alive and would
                // otherwise let two schedulers tick concurrently
                // against two different vaults.
                app_handle.manage(RebuildSlot(tokio::sync::Mutex::new(RebuildSlotInner {
                    orchestrator,
                    applied_vault_root: boot_settings.vault_root.trim().to_owned(),
                    auto_record_scheduler,
                })));
                // UI revamp PR 4: install the SSE bridge state slot.
                // The bridge task itself is started by the
                // `heron_subscribe_events` command on app mount.
                events_bridge::install(&app_handle);
                Ok::<_, Box<dyn std::error::Error>>(())
            })?;
            // Phase 68 (PR-ζ): register the saved hotkey at app
            // startup so the chord is live the moment the app
            // launches — not only when the user opens Settings →
            // Hotkey tab. Failures (e.g. another app already owns the
            // chord) are logged but don't block launch; the user can
            // pick a different chord in Settings without re-launching.
            //
            // Tier 4 #24: install the pending-conflicts buffer *before*
            // `register_startup_hotkey` so any conflicts surfaced
            // during this synchronous registration loop land in
            // managed state. The webview drains it on mount via
            // [`heron_take_pending_shortcut_conflicts`] — Tauri events
            // emitted from `setup` aren't reliably delivered because
            // the webview hasn't subscribed yet, so the buffer is the
            // canonical surface for cold-start conflicts.
            app.manage(shortcuts::PendingConflicts::default());
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
            heron_resummarize_preview,
            heron_update_action_item,
            heron_check_backup,
            heron_restore_backup,
            heron_test_microphone,
            heron_test_audio_tap,
            heron_test_accessibility,
            heron_test_calendar,
            heron_test_model_download,
            // Gap #5b: wire the real WhisperKit fetch (was a TODO
            // placeholder badge in the wizard's step 5).
            heron_download_model,
            // Gap #7 (this PR): in-process daemon liveness +
            // structured status surface.
            heron_test_daemon,
            heron_daemon_status,
            heron_mark_onboarded,
            heron_open_window,
            heron_open_vault_folder,
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
            // Tier 4 #24: cold-start drain for shortcut-registration
            // conflicts captured during the Tauri `setup` hook before
            // the webview was listening.
            heron_take_pending_shortcut_conflicts,
            heron_disk_usage,
            // Issue #226: frontend error reporting + ErrorBoundary
            // instrumentation. The renderer's ErrorBoundary fires
            // this fire-and-forget on `componentDidCatch`; the handler
            // bumps `frontend_errors_total{component, error_class}` on
            // the same Prometheus recorder #223 installed and logs the
            // structured payload via `tracing::warn!`.
            heron_report_frontend_error,
            heron_purge_audio_older_than,
            // Tier 4 #20 — summary retention sweeper. Sibling of the
            // audio sweeper above; consumes `Settings.summary_retention_days`.
            heron_purge_summaries_older_than,
            // Phase 73 (PR-λ) — pre-flight checks.
            heron_check_disk_for_recording,
            tray::heron_emit_capture_degraded,
            // Gap #6 — surface `heron-doctor`'s consolidated runtime
            // checks (ONNX, Zoom, keychain ACL, network) to the
            // onboarding wizard. The wizard's individual `heron_test_*`
            // probes stay in place; this one returns the cross-cutting
            // "is this machine ready to record?" verdict.
            heron_run_runtime_checks,
            // UI revamp PR 3: meetings list + summary proxy. Routes
            // `GET /v1/meetings` and `GET /v1/meetings/{id}/summary`
            // through Rust because the daemon's bearer auth + Origin
            // policy + the Tauri CSP all block direct webview access.
            meetings::heron_list_meetings,
            meetings::heron_get_meeting,
            meetings::heron_meeting_summary,
            meetings::heron_meeting_transcript,
            meetings::heron_meeting_audio,
            // Gap #7 recording-capture wiring: Start / Stop in the
            // desktop UI now actually drive the daemon's
            // `POST /v1/meetings` and `POST /v1/meetings/{id}/end`
            // endpoints (previously the buttons only flipped local
            // recording-store state). Same auth/Origin/CSP rationale
            // as the read proxies above.
            meetings::heron_start_capture,
            meetings::heron_end_meeting,
            // Tier 3 #16: pause/resume an in-progress capture. The
            // Recording page's Pause button funnels through here so
            // the daemon-side capture pipeline actually drops frames
            // (previously the button only flipped local React state,
            // and frames kept landing on disk).
            meetings::heron_pause_meeting,
            meetings::heron_resume_meeting,
            // Gap #8: backend-ready endpoints the desktop UI never
            // wired. `list_calendar_upcoming` powers the Home page's
            // upcoming-meetings rail; `attach_context` lets a click on
            // a calendar row pre-stage agenda + attendees before
            // start_capture so the orchestrator finds the briefing in
            // `pending_contexts` when the matching meeting arms. Same
            // auth/Origin/CSP rationale as the read proxies above.
            meetings::heron_list_calendar_upcoming,
            meetings::heron_attach_context,
            // Tier 5 #25: auto-prepare a minimal pre-meeting context
            // for every event surfaced by the rail's `ensureFresh`
            // pass. Daemon synthesizes a default `PreMeetingContext`
            // (today: just `attendees_known`); the rail renders a
            // "primed" indicator on each event card.
            meetings::heron_prepare_context,
            // Tier 5 #26: per-event auto-record flag. The daemon
            // scheduler auto-starts enabled calendar events as their
            // start windows open; the Home rail owns the row toggle.
            meetings::heron_set_event_auto_record,
            // UI revamp PR 4: Tauri-side SSE bridge for the daemon's
            // `/v1/events` stream. Same auth/Origin/CSP rationale as
            // the meetings proxy — the webview cannot connect
            // directly. The bridge is app-lifetime; the frontend
            // listens via @tauri-apps/api/event::listen("heron://event").
            events_bridge::heron_subscribe_events,
            events_bridge::heron_unsubscribe_events,
        ])
        // We split the original `.run(generate_context!())`
        // shorthand into `.build(...)?.run(callback)` so we can
        // observe Tauri lifecycle events — specifically
        // [`tauri::RunEvent::Exit`], which fires after the event
        // loop has drained but before the process exits. That
        // window is where we ask the in-process `herond` axum
        // service to begin its graceful-shutdown cleanup
        // (`with_graceful_shutdown` stops accepting new connections
        // and lets in-flight requests finish on a best-effort
        // basis). The Exit callback is sync, so we cannot `await`
        // the axum task to fully join here — the Tauri runtime
        // proceeds with its own teardown and may abort the task
        // before drain completes. For an in-process daemon serving
        // a co-tenanted WebView this is acceptable: the only client
        // is going away too. Also see [`tauri::RunEvent::ExitRequested`],
        // which fires *before* drain — we don't bind that one
        // because the user-driven close path (clicking Quit, Cmd+Q)
        // already routes through `Exit` and we don't want to
        // short-circuit the OS-level "are you sure?" dialog the
        // ExitRequested API would let us override.
        .build(tauri::generate_context!())
        .expect("error while building heron-desktop")
        .run(|app_handle, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                // Best-effort. The Exit callback is sync; reach into
                // the managed `DaemonHandle` and fire its oneshot.
                // Log if the handle is missing — that would mean
                // `daemon::install` never ran, which is a
                // programming bug we want to surface in the system
                // log rather than swallow.
                // UI revamp PR 4: cancel the SSE bridge first so its
                // streaming reqwest call doesn't hold the daemon's
                // axum graceful-shutdown waiting on a draining
                // response.
                events_bridge::shutdown_from_state(app_handle);
                if let Some(handle) = app_handle.try_state::<DaemonHandle>() {
                    handle.signal_shutdown();
                    tracing::info!("Exit hook: shutdown signaled to in-process herond");
                } else {
                    tracing::warn!("Exit hook: no DaemonHandle in state; daemon shutdown skipped",);
                }
            }
        });
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
    /// `resolve_vault_root` must mirror
    /// `crates/herond/src/main.rs::resolve_vault_root`'s precedence:
    /// non-empty env var wins, otherwise `~/heron-vault`. Empty /
    /// whitespace-only env var must be treated as unset (the
    /// `PathBuf::from("")` footgun otherwise resolves to CWD at
    /// runtime).
    ///
    /// We avoid mutating the env var itself (process-global; would
    /// race with the tray / event_bus tests in the same binary) and
    /// instead exercise the precedence directly via the helper's
    /// docstring contract: when `HERON_VAULT_ROOT` is unset, the
    /// returned path ends in `heron-vault`.
    #[test]
    fn resolve_vault_root_falls_back_to_heron_vault() {
        // Belt-and-suspenders: read the env var and skip if a parent
        // process set it. This stops a developer with the var
        // exported from getting a confusing red.
        if std::env::var_os("HERON_VAULT_ROOT").is_some() {
            eprintln!("skipped: HERON_VAULT_ROOT is set in this shell");
            return;
        }
        // On a sandboxed runner without a resolvable home dir the
        // fallback returns None; both are acceptable and pinned by
        // the docstring (so `if let Some` is the right shape — we
        // intentionally accept None as a no-op).
        if let Some(p) = resolve_vault_root() {
            assert!(
                p.ends_with("heron-vault"),
                "expected …/heron-vault, got {}",
                p.display()
            );
        }
    }

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

    /// `heron_open_vault_folder` reads `vault_root` from the on-disk
    /// settings file, validates it, and only then launches `open(1)`.
    /// These tests cover the rejection branches; the success path would
    /// actually spawn Finder, which is undesirable in a unit test.
    #[allow(clippy::expect_used, clippy::unwrap_used)]
    mod open_vault_folder {
        use super::*;
        use tempfile::NamedTempFile;

        fn write_settings_file(vault_root: &str) -> NamedTempFile {
            let s = Settings {
                vault_root: vault_root.to_owned(),
                ..Settings::default()
            };
            let file = NamedTempFile::new().expect("temp settings file");
            write_settings(file.path(), &s).expect("write settings");
            file
        }

        #[tokio::test]
        async fn rejects_empty_vault_root() {
            let f = write_settings_file("");
            let err = heron_open_vault_folder(f.path().to_string_lossy().into_owned())
                .await
                .unwrap_err();
            assert!(err.contains("not configured"), "got: {err}");
        }

        #[tokio::test]
        async fn rejects_whitespace_vault_root() {
            let f = write_settings_file("   ");
            let err = heron_open_vault_folder(f.path().to_string_lossy().into_owned())
                .await
                .unwrap_err();
            assert!(err.contains("not configured"), "got: {err}");
        }

        #[tokio::test]
        async fn rejects_missing_vault_path() {
            let f = write_settings_file("/nonexistent/heron/vault/cannot-exist-7c3a91d2");
            let err = heron_open_vault_folder(f.path().to_string_lossy().into_owned())
                .await
                .unwrap_err();
            assert!(err.contains("not found"), "got: {err}");
        }

        #[tokio::test]
        async fn rejects_file_vault_path() {
            let leaf = NamedTempFile::new().expect("temp leaf");
            let f = write_settings_file(&leaf.path().to_string_lossy());
            let err = heron_open_vault_folder(f.path().to_string_lossy().into_owned())
                .await
                .unwrap_err();
            assert!(err.contains("not a directory"), "got: {err}");
        }
    }

    /// Issue #206 integration coverage: pin that
    /// [`build_orchestrator_for_settings`] produces an orchestrator
    /// whose read endpoints scan the `Settings.vault_root` *we
    /// supplied*, not whatever was wired at boot. This is the core
    /// bug — pre-fix, the boot orchestrator's vault was frozen at
    /// setup-time and a subsequent settings change left the daemon
    /// reading from the old location while `heron_list_meetings` (the
    /// renderer's HTTP proxy) read from the new one.
    ///
    /// The test simulates the full "user changed vault" path at the
    /// orchestrator-rebuild seam:
    /// 1. Drop a real meeting note into vault A via `VaultWriter::finalize_session`
    ///    so the orchestrator's frontmatter parser has something to surface.
    /// 2. Drop a *different* meeting into vault B.
    /// 3. Build orchestrator from `Settings { vault_root: A }`; assert
    ///    `list_meetings` returns A's meeting only.
    /// 4. Build orchestrator from `Settings { vault_root: B }`; assert
    ///    it returns B's meeting only.
    ///
    /// We don't go through `heron_write_settings` end-to-end because
    /// that path also rebinds the in-process daemon onto port 7384 —
    /// which would conflict with any concurrently-running test (or a
    /// developer's local `herond`). The seam this test exercises is
    /// the same `Arc<LocalSessionOrchestrator>` the rebuild path
    /// eventually feeds into [`crate::daemon::bind_after_rebuild`]; a regression
    /// that makes the helper ignore `Settings.vault_root` (e.g.
    /// always falling back to `resolve_vault_root`) would surface
    /// here exactly as it would surface in a manual repro.
    #[allow(clippy::expect_used, clippy::unwrap_used)]
    mod orchestrator_rebuild_for_vault {
        use super::*;
        use heron_session::{ListMeetingsQuery, SessionOrchestrator};
        use heron_types::{
            Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter, MeetingType,
        };
        use heron_vault::VaultWriter;
        use std::path::PathBuf as StdPathBuf;
        use tempfile::TempDir;

        fn baseline_frontmatter(company: &str) -> Frontmatter {
            Frontmatter {
                date: chrono::NaiveDate::from_ymd_opt(2026, 4, 24).expect("date"),
                start: "14:00".into(),
                duration_min: 30,
                company: Some(company.to_owned()),
                attendees: vec![],
                meeting_type: MeetingType::Client,
                source_app: "us.zoom.xos".into(),
                recording: StdPathBuf::from("recordings/2026-04-24-1400.m4a"),
                transcript: StdPathBuf::from("transcripts/2026-04-24-1400.jsonl"),
                diarize_source: DiarizeSource::Ax,
                disclosed: Disclosure {
                    stated: true,
                    when: Some("00:14".into()),
                    how: DisclosureHow::Verbal,
                },
                cost: Cost {
                    summary_usd: 0.01,
                    tokens_in: 100,
                    tokens_out: 50,
                    model: "test".into(),
                },
                action_items: vec![],
                tags: vec![],
                extra: serde_yaml::Mapping::default(),
            }
        }

        fn seed_meeting(vault: &Path, slug: &str, company: &str) {
            let writer = VaultWriter::new(vault);
            writer
                .finalize_session(
                    "2026-04-24",
                    "1400",
                    slug,
                    &baseline_frontmatter(company),
                    "Body.\n",
                )
                .expect("finalize meeting note");
        }

        fn settings_with_vault(vault: &Path) -> Settings {
            Settings {
                vault_root: vault.to_string_lossy().into_owned(),
                ..Settings::default()
            }
        }

        #[tokio::test]
        async fn orchestrator_built_from_settings_reads_supplied_vault() {
            // Vault A has a meeting from "Acme"; vault B has one from
            // "Globex". The orchestrator's `list_meetings` surfaces the
            // company name in `Meeting.title` (per
            // `crates/heron-orchestrator/src/vault_read.rs::meeting_from_note`),
            // so we can distinguish which vault the orchestrator is
            // pointed at by inspecting the title — a far more robust
            // assertion than count alone, since count would also pass
            // if the orchestrator silently fell back to a third
            // (empty) directory.
            let vault_a = TempDir::new().expect("tmp vault A");
            let vault_b = TempDir::new().expect("tmp vault B");
            seed_meeting(vault_a.path(), "acme-pricing", "Acme");
            seed_meeting(vault_b.path(), "globex-kickoff", "Globex");

            // Orchestrator A — the boot equivalent.
            let orch_a = build_orchestrator_for_settings(&settings_with_vault(vault_a.path()));
            let page_a = orch_a
                .list_meetings(ListMeetingsQuery::default())
                .await
                .expect("list_meetings on A");
            assert_eq!(page_a.items.len(), 1, "expected one A-meeting");
            assert_eq!(
                page_a.items[0].title.as_deref(),
                Some("Acme"),
                "vault A orchestrator surfaced wrong title: {:?}",
                page_a.items[0].title,
            );

            // Orchestrator B — the post-vault-swap rebuild equivalent.
            // This is the core of issue #206: a fresh
            // `build_orchestrator_for_settings` against *new* settings
            // must read from the new vault, not the boot one.
            let orch_b = build_orchestrator_for_settings(&settings_with_vault(vault_b.path()));
            let page_b = orch_b
                .list_meetings(ListMeetingsQuery::default())
                .await
                .expect("list_meetings on B");
            assert_eq!(page_b.items.len(), 1, "expected one B-meeting");
            assert_eq!(
                page_b.items[0].title.as_deref(),
                Some("Globex"),
                "vault B orchestrator surfaced wrong title: {:?}",
                page_b.items[0].title,
            );

            // Belt-and-suspenders: a meeting that lives in vault A is
            // *not* visible from orchestrator B. A regression that
            // accidentally shared a single vault root across rebuilt
            // orchestrators (the pre-fix bug, in spirit) would let
            // Acme leak through here.
            assert!(
                !page_b
                    .items
                    .iter()
                    .any(|m| m.title.as_deref() == Some("Acme")),
                "vault B orchestrator must not surface vault A's meetings",
            );
        }

        /// An empty `Settings.vault_root` reverts to the
        /// `resolve_vault_root` precedence (env var > `~/heron-vault`).
        /// In the sandboxed test process, with `HERON_VAULT_ROOT`
        /// unset and a resolvable home dir, the orchestrator points at
        /// `~/heron-vault`; in CI without a home dir it's
        /// substrate-only. Either way: a vault we just-seeded under a
        /// fresh tempdir must NOT show up — confirming that an
        /// "untrim me" wire value still rebuilds the orchestrator at
        /// the documented fallback path.
        #[tokio::test]
        async fn orchestrator_built_with_empty_vault_root_does_not_read_arbitrary_vault() {
            let unrelated_vault = TempDir::new().expect("tmp unrelated vault");
            seed_meeting(unrelated_vault.path(), "decoy", "Decoy");

            let orch = build_orchestrator_for_settings(&Settings {
                vault_root: "   ".into(),
                ..Settings::default()
            });
            // Either the orchestrator reads from `~/heron-vault` (and
            // the decoy meeting is invisible because it's under a
            // tempdir) or it's substrate-only (`NotYetImplemented`).
            // Both are acceptable; the bug we're guarding against is
            // "orchestrator silently picked up the renderer-supplied
            // tempdir despite the empty-string sentinel", which would
            // surface as the decoy title appearing in the page.
            match orch.list_meetings(ListMeetingsQuery::default()).await {
                Ok(page) => {
                    assert!(
                        !page
                            .items
                            .iter()
                            .any(|m| m.title.as_deref() == Some("Decoy")),
                        "empty vault_root must not leak the decoy tempdir into the orchestrator",
                    );
                }
                Err(_) => {
                    // Substrate-only / `NotYetImplemented` is the
                    // honest answer when the home dir is unresolvable
                    // and no captures are in flight. Acceptable.
                }
            }
        }
    }
}
