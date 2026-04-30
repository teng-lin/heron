//! Tier 4 #24: action-id–keyed global-shortcut registry.
//!
//! At app startup we iterate [`Settings::shortcuts`] and register each
//! entry through `tauri-plugin-global-shortcut`. Each registered chord
//! emits a Tauri event named `shortcut:<action_id>` so the renderer can
//! `listen("shortcut:toggle_recording", ...)` without caring which
//! accelerator string the user picked.
//!
//! ## Backward compatibility
//!
//! Pre–Tier-4 builds only registered [`Settings::record_hotkey`] and
//! emitted [`crate::EVENT_HOTKEY_FIRED`] (`"hotkey:fired"`). To avoid
//! regressing users whose `settings.json` predates the [`shortcuts`]
//! map, we treat `record_hotkey` as the default for the canonical
//! [`ACTION_TOGGLE_RECORDING`] action id and let an explicit
//! `shortcuts.toggle_recording` entry override it. The legacy
//! `hotkey:fired` event continues to fire alongside the new
//! `shortcut:toggle_recording` event because the plugin's global
//! `with_handler` (set up in `lib::run`) emits `hotkey:fired` for
//! every chord this app owns — independent of how the chord was
//! registered. Pre–Tier-4 frontend listeners keep working unchanged.
//!
//! ## Conflict + invalid-accelerator handling
//!
//! Two action ids mapping to the same accelerator (modulo modifier
//! ordering — `"Cmd+Shift+R"` and `"Shift+Cmd+R"` collide) is a
//! configuration error the user can usually only hit by hand-editing
//! `settings.json`. We log a `tracing::warn!` and skip the second
//! registration; iteration order is the [`BTreeMap`]'s sorted-by-key
//! order so the choice of "first wins" is deterministic across
//! launches.
//!
//! An accelerator string the plugin rejects (parse error or already
//! owned by another app) is logged at warn and skipped — one bad
//! entry must not abort the whole startup loop and lock the user out
//! of every other shortcut.

use std::collections::{BTreeMap, HashSet};

use tauri::{Emitter, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// Canonical action id for the Start/Stop Recording chord.
///
/// Pre–Tier-4 settings only had a single
/// [`crate::Settings::record_hotkey`] field; this id is the bridge
/// between that legacy field and the new
/// [`crate::Settings::shortcuts`] map. The Settings UI's Hotkey tab
/// continues to drive `record_hotkey`; the new shortcuts table can
/// override it by writing the same id with a different accelerator.
pub const ACTION_TOGGLE_RECORDING: &str = "toggle_recording";

/// Tauri event-name prefix for shortcut firings.
///
/// The full event name is `shortcut:<action_id>` (e.g.
/// `shortcut:toggle_recording`). Listeners on the renderer side use
/// the action id rather than the accelerator string so they remain
/// stable when the user rebinds the chord.
pub const EVENT_PREFIX: &str = "shortcut:";

/// Tauri event name for the one-time "two action ids share the same
/// accelerator" toast surface. Payload is the conflicting accelerator
/// string and the two action ids that collided. Frontend renders one
/// Sonner toast per launch — quiet enough that a real user-edited
/// `settings.json` mistake is surfaced without spamming.
pub const EVENT_CONFLICT: &str = "shortcut:conflict";

/// Wire payload for [`EVENT_CONFLICT`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConflictNotice {
    /// The accelerator string that two actions both mapped to.
    pub accelerator: String,
    /// The action id that won the registration (sorted-key first).
    pub kept: String,
    /// The action id that was skipped.
    pub skipped: String,
}

/// Build the canonical `(action_id, accelerator)` list to register at
/// startup, with `record_hotkey` as the default for
/// [`ACTION_TOGGLE_RECORDING`] and an explicit `shortcuts` entry
/// overriding it.
///
/// Pure function so the merge contract is unit-testable without a
/// Tauri runtime. Accelerator strings are returned as-is; conflict
/// detection happens later in [`register_all`] after the plugin has
/// parsed each one (so `"Cmd+Shift+R"` and `"Shift+Cmd+R"` collide
/// even though the strings differ).
///
/// Iteration order:
/// 1. `toggle_recording` first (so its slot is established before any
///    conflicting entry could displace it via sort-order — first wins).
/// 2. Remaining shortcuts in [`BTreeMap`] sorted-key order.
///
/// An empty `record_hotkey` and a missing `shortcuts.toggle_recording`
/// together mean "no toggle chord" — the entry is omitted, matching
/// the existing `record_hotkey.is_empty()` short-circuit in the
/// pre-Tier-4 startup path.
pub fn resolve_registrations(
    record_hotkey: &str,
    shortcuts: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(shortcuts.len() + 1);

    // toggle_recording: explicit shortcuts entry wins, else the legacy
    // record_hotkey field is the default.
    let toggle = shortcuts
        .get(ACTION_TOGGLE_RECORDING)
        .map(String::as_str)
        .unwrap_or(record_hotkey);
    if !toggle.is_empty() {
        out.push((ACTION_TOGGLE_RECORDING.to_owned(), toggle.to_owned()));
    }

    // Remaining shortcuts in sorted-key order (BTreeMap iteration is
    // already sorted; we just skip the toggle entry we already
    // handled). An empty accelerator is treated as "unbound" and
    // skipped — same semantic as the legacy `record_hotkey` empty-
    // string short-circuit.
    for (action_id, accel) in shortcuts {
        if action_id == ACTION_TOGGLE_RECORDING || accel.is_empty() {
            continue;
        }
        out.push((action_id.clone(), accel.clone()));
    }

    out
}

/// Per-entry outcome from [`register_all`]. Exposed for tests; the
/// production caller doesn't need to inspect the result list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationOutcome {
    /// Successfully registered with the OS.
    Registered,
    /// Skipped because an earlier action_id already owns this
    /// accelerator (conflict; first-write-wins).
    SkippedConflict { kept_action_id: String },
    /// The plugin rejected the accelerator string (parse error or
    /// platform-level conflict with another app).
    InvalidAccelerator(String),
}

/// Register every entry from [`resolve_registrations`] with the
/// global-shortcut plugin and emit `shortcut:<action_id>` on each
/// firing.
///
/// Returns the per-action outcomes in registration order so callers
/// (and tests) can assert on conflict / invalid-accel handling. The
/// real `lib::run` setup hook ignores the return value; logging is
/// the user-visible signal.
///
/// Errors from the plugin's `on_shortcut` are non-fatal — one bad
/// chord must not lock the user out of the rest. Conflicts (same
/// parsed [`Shortcut::id`] reached from two different action ids) are
/// resolved first-wins by iteration order, which is the
/// [`BTreeMap`]'s sorted key order.
pub fn register_all<R: Runtime>(
    app: &tauri::AppHandle<R>,
    record_hotkey: &str,
    shortcuts: &BTreeMap<String, String>,
) -> Vec<(String, RegistrationOutcome)> {
    let entries = resolve_registrations(record_hotkey, shortcuts);
    register_with(app, &entries, |handle, action_id, accel| {
        let manager = handle.global_shortcut();
        let action_id_owned = action_id.to_owned();
        manager.on_shortcut(accel, move |app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                emit_for_action(app, &action_id_owned);
            }
        })
    })
}

/// Inner driver of [`register_all`] that delegates the actual plugin
/// call to a closure. Threading the registration through a closure
/// lets tests assert on conflict / parse-error handling without
/// standing up a real `tauri-plugin-global-shortcut` instance (the
/// plugin's `setup` hook spawns a platform global-hotkey daemon that
/// would interfere with concurrent tests).
fn register_with<R, F>(
    app: &tauri::AppHandle<R>,
    entries: &[(String, String)],
    mut do_register: F,
) -> Vec<(String, RegistrationOutcome)>
where
    R: Runtime,
    F: FnMut(&tauri::AppHandle<R>, &str, &str) -> Result<(), tauri_plugin_global_shortcut::Error>,
{
    // Track which canonical Shortcut ids we've already registered so a
    // second action_id mapping to the same chord (modulo modifier
    // ordering) is detected and skipped. Map: parsed-id -> winning
    // action_id (used for the warn message + ConflictNotice payload).
    let mut seen: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    // Track which action_ids we've emitted a one-time conflict toast
    // for, so a misconfigured settings.json with three colliding
    // entries surfaces only one toast per losing entry (no spam).
    let mut conflict_emitted: HashSet<String> = HashSet::new();
    let mut outcomes = Vec::with_capacity(entries.len());

    for (action_id, accel) in entries {
        // Parse the accelerator first so we can canonicalize for
        // conflict detection. A parse failure here is the same as a
        // plugin reject — log + skip + continue.
        let parsed: Shortcut = match accel.parse() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    action_id = %action_id,
                    accelerator = %accel,
                    error = %e,
                    "shortcuts: skipping invalid accelerator",
                );
                outcomes.push((
                    action_id.clone(),
                    RegistrationOutcome::InvalidAccelerator(e.to_string()),
                ));
                continue;
            }
        };
        let canonical_id = parsed.id();

        if let Some(kept) = seen.get(&canonical_id).cloned() {
            tracing::warn!(
                action_id = %action_id,
                accelerator = %accel,
                kept_action_id = %kept,
                "shortcuts: accelerator already bound to another action; skipping",
            );
            // One toast per losing entry, fired best-effort. A
            // missing main window during shutdown silently drops the
            // event rather than panicking.
            if conflict_emitted.insert(action_id.clone()) {
                let _ = app.emit(
                    EVENT_CONFLICT,
                    ConflictNotice {
                        accelerator: accel.clone(),
                        kept: kept.clone(),
                        skipped: action_id.clone(),
                    },
                );
            }
            outcomes.push((
                action_id.clone(),
                RegistrationOutcome::SkippedConflict {
                    kept_action_id: kept,
                },
            ));
            continue;
        }

        match do_register(app, action_id, accel) {
            Ok(()) => {
                seen.insert(canonical_id, action_id.clone());
                outcomes.push((action_id.clone(), RegistrationOutcome::Registered));
            }
            Err(e) => {
                tracing::warn!(
                    action_id = %action_id,
                    accelerator = %accel,
                    error = %e,
                    "shortcuts: plugin rejected registration; continuing",
                );
                outcomes.push((
                    action_id.clone(),
                    RegistrationOutcome::InvalidAccelerator(e.to_string()),
                ));
            }
        }
    }

    outcomes
}

/// Emit the `shortcut:<action_id>` event for `action_id`.
///
/// Backward-compat for the legacy `hotkey:fired` event is handled by
/// the plugin's global `with_handler` registered in `lib::run` — that
/// fires for *every* chord regardless of how it was registered, so
/// pre–Tier-4 listeners on `hotkey:fired` keep working without code
/// changes here.
fn emit_for_action<R: Runtime>(app: &tauri::AppHandle<R>, action_id: &str) {
    tracing::info!(action_id = %action_id, "shortcut fired");
    let event_name = format!("{EVENT_PREFIX}{action_id}");
    let _ = app.emit(&event_name, ());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcuts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // ---------- resolve_registrations ----------

    #[test]
    fn empty_shortcuts_with_record_hotkey_yields_toggle_only() {
        let out = resolve_registrations("CmdOrCtrl+Shift+R", &BTreeMap::new());
        assert_eq!(
            out,
            vec![(
                ACTION_TOGGLE_RECORDING.to_owned(),
                "CmdOrCtrl+Shift+R".to_owned()
            )]
        );
    }

    #[test]
    fn empty_record_hotkey_and_empty_shortcuts_yields_nothing() {
        let out = resolve_registrations("", &BTreeMap::new());
        assert!(out.is_empty(), "got: {out:?}");
    }

    #[test]
    fn shortcuts_toggle_recording_overrides_record_hotkey() {
        let map = shortcuts(&[(ACTION_TOGGLE_RECORDING, "CmdOrCtrl+Alt+T")]);
        let out = resolve_registrations("CmdOrCtrl+Shift+R", &map);
        assert_eq!(
            out,
            vec![(
                ACTION_TOGGLE_RECORDING.to_owned(),
                "CmdOrCtrl+Alt+T".to_owned()
            )]
        );
    }

    #[test]
    fn additional_actions_appear_in_sorted_order_after_toggle() {
        // BTreeMap iteration is sorted, so given keys ["pause", "summarize"]
        // the resolved list is [toggle, pause, summarize].
        let map = shortcuts(&[
            ("summarize_now", "CmdOrCtrl+Alt+S"),
            ("pause_resume", "CmdOrCtrl+Alt+P"),
        ]);
        let out = resolve_registrations("CmdOrCtrl+Shift+R", &map);
        assert_eq!(
            out,
            vec![
                (
                    ACTION_TOGGLE_RECORDING.to_owned(),
                    "CmdOrCtrl+Shift+R".to_owned()
                ),
                ("pause_resume".to_owned(), "CmdOrCtrl+Alt+P".to_owned()),
                ("summarize_now".to_owned(), "CmdOrCtrl+Alt+S".to_owned()),
            ]
        );
    }

    #[test]
    fn empty_accelerator_string_in_shortcuts_is_skipped() {
        // A user clearing a chord in the Settings UI may persist as
        // an empty string rather than a removed key. Treat empty-
        // string the same as "unbound" — same semantic as the legacy
        // record_hotkey empty short-circuit.
        let map = shortcuts(&[("pause_resume", "")]);
        let out = resolve_registrations("CmdOrCtrl+Shift+R", &map);
        assert_eq!(
            out,
            vec![(
                ACTION_TOGGLE_RECORDING.to_owned(),
                "CmdOrCtrl+Shift+R".to_owned()
            )]
        );
    }

    #[test]
    fn explicit_empty_toggle_in_shortcuts_means_unbound() {
        // shortcuts.toggle_recording = "" overrides record_hotkey
        // with "no chord". The user cleared it explicitly.
        let map = shortcuts(&[(ACTION_TOGGLE_RECORDING, "")]);
        let out = resolve_registrations("CmdOrCtrl+Shift+R", &map);
        assert!(out.is_empty(), "got: {out:?}");
    }

    // ---------- register_with: conflict + invalid-accel ----------

    /// Drive `register_with` with a stub registrar so we can assert on
    /// the per-entry outcomes without standing up
    /// `tauri-plugin-global-shortcut`.
    fn run_with_stub<F>(
        entries: &[(String, String)],
        registrar: F,
    ) -> Vec<(String, RegistrationOutcome)>
    where
        F: FnMut(
            &tauri::AppHandle<tauri::test::MockRuntime>,
            &str,
            &str,
        ) -> Result<(), tauri_plugin_global_shortcut::Error>,
    {
        let app = tauri::test::mock_app();
        register_with(app.handle(), entries, registrar)
    }

    #[test]
    fn second_action_with_same_accelerator_is_skipped_first_wins() {
        let entries = vec![
            (
                "toggle_recording".to_owned(),
                "CmdOrCtrl+Shift+R".to_owned(),
            ),
            ("pause_resume".to_owned(), "CmdOrCtrl+Shift+R".to_owned()),
        ];
        let mut calls: Vec<String> = Vec::new();
        let outcomes = run_with_stub(&entries, |_app, action_id, _accel| {
            calls.push(action_id.to_owned());
            Ok(())
        });

        assert_eq!(
            calls,
            vec!["toggle_recording".to_owned()],
            "the second (conflicting) entry must not reach the plugin",
        );
        assert_eq!(
            outcomes,
            vec![
                (
                    "toggle_recording".to_owned(),
                    RegistrationOutcome::Registered
                ),
                (
                    "pause_resume".to_owned(),
                    RegistrationOutcome::SkippedConflict {
                        kept_action_id: "toggle_recording".to_owned(),
                    },
                ),
            ],
        );
    }

    #[test]
    fn conflict_detection_canonicalizes_modifier_order() {
        // "Cmd+Shift+R" and "Shift+Cmd+R" parse to the same Shortcut
        // id, so the second must be flagged as a conflict even though
        // the literal strings differ.
        let entries = vec![
            ("toggle_recording".to_owned(), "Cmd+Shift+R".to_owned()),
            ("pause_resume".to_owned(), "Shift+Cmd+R".to_owned()),
        ];
        let outcomes = run_with_stub(&entries, |_, _, _| Ok(()));
        assert!(matches!(
            outcomes[1].1,
            RegistrationOutcome::SkippedConflict { .. }
        ));
    }

    #[test]
    fn invalid_accelerator_is_non_fatal() {
        // First entry parse-fails; second registers normally. The
        // loop must not abort on the first bad entry.
        let entries = vec![
            ("bogus_action".to_owned(), "NotAValidChord".to_owned()),
            (
                "toggle_recording".to_owned(),
                "CmdOrCtrl+Shift+R".to_owned(),
            ),
        ];
        let mut calls: Vec<String> = Vec::new();
        let outcomes = run_with_stub(&entries, |_, action_id, _| {
            calls.push(action_id.to_owned());
            Ok(())
        });
        assert!(matches!(
            outcomes[0].1,
            RegistrationOutcome::InvalidAccelerator(_)
        ));
        assert_eq!(outcomes[1].1, RegistrationOutcome::Registered);
        assert_eq!(
            calls,
            vec!["toggle_recording".to_owned()],
            "the bogus accelerator must never reach the plugin",
        );
    }

    #[test]
    fn plugin_register_error_is_non_fatal_and_continues() {
        // Simulate the plugin returning Err for the first entry (e.g.
        // the OS reports another app already owns it). The second
        // entry must still be attempted.
        let entries = vec![
            (
                "toggle_recording".to_owned(),
                "CmdOrCtrl+Shift+R".to_owned(),
            ),
            ("pause_resume".to_owned(), "CmdOrCtrl+Alt+P".to_owned()),
        ];
        let mut call_count = 0;
        let outcomes = run_with_stub(&entries, |_, _, _| {
            call_count += 1;
            if call_count == 1 {
                Err(tauri_plugin_global_shortcut::Error::GlobalHotkey(
                    "platform: chord already taken".to_owned(),
                ))
            } else {
                Ok(())
            }
        });
        assert!(matches!(
            outcomes[0].1,
            RegistrationOutcome::InvalidAccelerator(_)
        ));
        assert_eq!(outcomes[1].1, RegistrationOutcome::Registered);
        // The first entry's failed registration must NOT be recorded
        // as "seen" — otherwise a re-attempt at the same chord under
        // a different action would incorrectly trigger the conflict
        // path. Pin that by registering the same accel a second time.
        let entries2 = vec![
            ("a".to_owned(), "CmdOrCtrl+Shift+X".to_owned()),
            ("b".to_owned(), "CmdOrCtrl+Shift+X".to_owned()),
        ];
        let mut count = 0;
        let outcomes2 = run_with_stub(&entries2, |_, _, _| {
            count += 1;
            if count == 1 {
                Err(tauri_plugin_global_shortcut::Error::GlobalHotkey(
                    "boom".to_owned(),
                ))
            } else {
                Ok(())
            }
        });
        assert!(matches!(
            outcomes2[0].1,
            RegistrationOutcome::InvalidAccelerator(_)
        ));
        assert_eq!(
            outcomes2[1].1,
            RegistrationOutcome::Registered,
            "a failed registration must not occupy the conflict slot",
        );
    }

    #[test]
    fn record_hotkey_fallback_preserves_existing_single_shortcut_behavior() {
        // Empty shortcuts map + non-empty record_hotkey: exactly one
        // entry attempted, with action id ACTION_TOGGLE_RECORDING.
        // This is the regression gate for "users without a populated
        // shortcuts map keep their record-hotkey behavior".
        let entries = resolve_registrations("CmdOrCtrl+Shift+R", &BTreeMap::new());
        let mut calls: Vec<(String, String)> = Vec::new();
        let outcomes = run_with_stub(&entries, |_, action_id, accel| {
            calls.push((action_id.to_owned(), accel.to_owned()));
            Ok(())
        });
        assert_eq!(
            calls,
            vec![(
                ACTION_TOGGLE_RECORDING.to_owned(),
                "CmdOrCtrl+Shift+R".to_owned()
            )]
        );
        assert_eq!(
            outcomes,
            vec![(
                ACTION_TOGGLE_RECORDING.to_owned(),
                RegistrationOutcome::Registered
            )]
        );
    }
}
