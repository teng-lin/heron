//! macOS menubar tray icon for the heron desktop shell.
//!
//! Phase 64 (PR-β) adds a status-bar tray that mirrors the recording
//! FSM (`heron_types::RecordingState`) and exposes a small dropdown
//! menu for the most-used affordances:
//!
//!   - **Open last note…** — picks the newest `*.md` in the vault,
//!     emits `nav:review` with the basename so the React tree
//!     navigates to `/review/<id>`. Phase 69 (PR-η) replaced the
//!     PR-β stub.
//!   - **Settings…**       — focuses the main window and emits a
//!     `nav:settings` event the React tree converts to `useNavigate()`.
//!   - **Quit**            — terminates the Tauri app.
//!
//! Implementation notes
//! ====================
//!
//! - **Polling, not push.** The tray reads `heron_status` on a 1 s
//!   `tokio::time::interval`. The orchestrator does not yet emit FSM
//!   transitions over an event bus, and adding one is out of scope for
//!   PR-β. 1 s is the latency budget the brief calls for and is well
//!   under the human-perceptible threshold for menubar updates.
//! - **Template icons.** The five PNGs in `icons/tray/` are 22×22
//!   monochrome shapes on a transparent background, with @2x variants
//!   for HiDPI. We pass `set_icon_as_template(true)` so macOS replaces
//!   the RGB channel with its menubar tint. The PNGs themselves are
//!   placeholders (PR body documents this) — the iconography is a
//!   follow-up design task.
//! - **Bundling.** `include_bytes!` embeds the PNGs into the binary so
//!   the tray works regardless of where the `.app` bundle is installed
//!   and survives `cargo run`/`tauri dev` from arbitrary cwds.

use std::time::Duration;

use tauri::menu::{Menu, MenuEvent, MenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager, Runtime, image::Image};
use tokio::time::interval;

use heron_types::RecordingState;

/// IDs we use for the menu items so `MenuEvent`s carry a stable tag.
const MENU_OPEN_LAST: &str = "open_last_note";
const MENU_SETTINGS: &str = "open_settings";
const MENU_QUIT: &str = "quit";

/// Frontend event names. `App.tsx` listens for these via
/// `@tauri-apps/api/event::listen` and threads them into
/// `react-router::useNavigate()`.
const EVENT_NAV_SETTINGS: &str = "nav:settings";
const EVENT_NAV_RECORDING: &str = "nav:recording";
/// Phase 69: payload `{ sessionId: string }` so the listener can
/// navigate to `/review/<id>`. Pure-route events (settings/recording)
/// stay payloadless to avoid disturbing PR-β's hook.
pub const EVENT_NAV_REVIEW: &str = "nav:review";
/// Phase 69: emitted when "Open last note…" runs and the vault is
/// empty. The frontend pops a Sonner toast — keeps the tray's
/// system-notification line free of platform-specific plumbing while
/// still surfacing the "no notes yet" affordance to the user.
pub const EVENT_NO_LAST_NOTE: &str = "nav:no_last_note";

/// Embedded icon bytes — 22×22 PNGs (44×44 @2x). The `@2x` files exist
/// for HiDPI but tray-icon on macOS picks the right variant by hash,
/// not by filename, so we embed the 1× set and let AppKit handle DPI
/// scaling of the template image.
const ICON_IDLE: &[u8] = include_bytes!("../icons/tray/idle.png");
const ICON_RECORDING: &[u8] = include_bytes!("../icons/tray/recording.png");
const ICON_TRANSCRIBING: &[u8] = include_bytes!("../icons/tray/transcribing.png");
const ICON_SUMMARIZING: &[u8] = include_bytes!("../icons/tray/summarizing.png");
const ICON_ERROR: &[u8] = include_bytes!("../icons/tray/error.png");

/// What to render in the tray.
///
/// Maps from `RecordingState` plus an "error" kind that the FSM does
/// not yet model — for now we never emit it from the polling loop, but
/// the variant is here so a future error-channel can flip the tray to
/// red without growing the API surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayVisual {
    Idle,
    Recording,
    Transcribing,
    Summarizing,
    #[allow(dead_code)] // reserved for the error path; see module docs
    Error,
}

impl TrayVisual {
    fn from_state(state: RecordingState) -> Self {
        match state {
            // The "armed" / "armed_cooldown" states are pre-recording
            // affordances driven by the consent gate; visually they
            // look idle in the tray (we haven't started capturing).
            RecordingState::Idle | RecordingState::Armed | RecordingState::ArmedCooldown => {
                Self::Idle
            }
            RecordingState::Recording => Self::Recording,
            RecordingState::Transcribing => Self::Transcribing,
            RecordingState::Summarizing => Self::Summarizing,
        }
    }

    fn icon_bytes(self) -> &'static [u8] {
        match self {
            Self::Idle => ICON_IDLE,
            Self::Recording => ICON_RECORDING,
            Self::Transcribing => ICON_TRANSCRIBING,
            Self::Summarizing => ICON_SUMMARIZING,
            Self::Error => ICON_ERROR,
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Idle => "heron — idle",
            Self::Recording => "heron — recording",
            Self::Transcribing => "heron — transcribing",
            Self::Summarizing => "heron — summarizing",
            Self::Error => "heron — error",
        }
    }
}

/// Build the dropdown menu. Returns the menu plus a closure-friendly
/// owned handle to attach to the `TrayIconBuilder`.
fn build_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let open_last = MenuItem::with_id(app, MENU_OPEN_LAST, "Open last note…", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, MENU_SETTINGS, "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "Quit", true, None::<&str>)?;

    Menu::with_items(app, &[&open_last, &settings, &quit])
}

/// Reveal and focus the main webview window. macOS may have minimised
/// or hidden it; we want the user to land on the React tree, ready to
/// react to whatever event we just emitted.
fn focus_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        // Show + unminimize + focus. Each call is best-effort: a
        // failure here is not fatal (the tray icon's job is signalling,
        // not window management), and `unwrap()` is forbidden by the
        // workspace clippy config anyway.
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// Dispatch a menu event. Pulled out of the `on_menu_event` closure so
/// the handler stays small and the unit tests below can drive the same
/// dispatch path without spinning up Tauri.
fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, event: &MenuEvent) {
    match event.id().as_ref() {
        MENU_OPEN_LAST => {
            // Phase 69 (PR-η): resolve the newest `*.md` in the user's
            // configured vault and route the frontend to its review
            // page. Reading settings / walking the vault must not
            // block the tray's UI thread, so we spawn onto Tauri's
            // async runtime — the lookup is bounded to one `read_dir`.
            let app_handle = app.clone();
            tauri::async_runtime::spawn(async move {
                open_last_note_dispatch(&app_handle);
            });
        }
        MENU_SETTINGS => {
            focus_main_window(app);
            // `()` is the canonical empty payload — `Emitter::emit`
            // serialises it to JSON `null`, which the JS listener
            // ignores. No allocation per dispatch.
            if let Err(err) = app.emit(EVENT_NAV_SETTINGS, ()) {
                eprintln!("[heron-tray] failed to emit {EVENT_NAV_SETTINGS}: {err}");
            }
        }
        MENU_QUIT => {
            app.exit(0);
        }
        other => {
            eprintln!("[heron-tray] unknown menu event id: {other}");
        }
    }
}

/// Spawn the 1 s polling loop that mirrors the recording FSM into the
/// tray's icon + tooltip.
///
/// The loop runs until the Tauri runtime shuts down. On a decode or
/// render error we leave the previous icon in place rather than
/// flashing the error variant — a transient failure shouldn't repaint
/// the menubar red. The `Error` visual is reserved for a future
/// explicit error event channel.
///
/// **Phase 64 caveat.** The FSM today is constructed lazily inside
/// `heron_status` and isn't long-lived: every tick observes a fresh
/// `RecordingFsm::new()`, which is always `Idle`. The poll loop is
/// scaffold for the right wire-up shape — once the orchestrator owns
/// a shared FSM in Tauri app-state, the body of this loop swaps to a
/// `State::<RecordingFsm>` lookup with no caller change. Until then
/// the tray sticks on `Idle` after the initial paint.
fn spawn_status_poll<R: Runtime>(tray: TrayIcon<R>) {
    tauri::async_runtime::spawn(async move {
        let mut tick = interval(Duration::from_secs(1));
        let mut last_visual: Option<TrayVisual> = None;
        loop {
            tick.tick().await;
            let fsm = heron_types::RecordingFsm::new();
            let visual = TrayVisual::from_state(fsm.state());
            if Some(visual) == last_visual {
                continue;
            }
            last_visual = Some(visual);
            apply_visual(&tray, visual);
        }
    });
}

/// Push a `TrayVisual` to the live tray icon. Logs (without panicking)
/// if the runtime rejects either of the two updates — this keeps the
/// poller alive across transient errors instead of tearing down.
fn apply_visual<R: Runtime>(tray: &TrayIcon<R>, visual: TrayVisual) {
    match Image::from_bytes(visual.icon_bytes()) {
        Ok(image) => {
            if let Err(err) = tray.set_icon(Some(image)) {
                eprintln!("[heron-tray] set_icon failed: {err}");
            }
        }
        Err(err) => {
            eprintln!("[heron-tray] decode icon failed: {err}");
        }
    }
    if let Err(err) = tray.set_tooltip(Some(visual.tooltip())) {
        eprintln!("[heron-tray] set_tooltip failed: {err}");
    }
}

/// Install the tray icon. Call this from `setup(...)` exactly once.
///
/// On non-macOS the tray will still build (Tauri 2 supports tray on
/// Linux + Windows too), but the brief is macOS-first; the call is
/// gated only by the workspace's overall macOS-only product target,
/// not by an extra `cfg`.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let menu = build_menu(app)?;
    let initial_visual = TrayVisual::Idle;
    let initial_image = Image::from_bytes(initial_visual.icon_bytes())?;

    let tray = TrayIconBuilder::new()
        .icon(initial_image)
        .icon_as_template(true)
        .tooltip(initial_visual.tooltip())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| handle_menu_event(app, &event))
        .build(app)?;

    spawn_status_poll(tray);
    Ok(())
}

/// Tauri command: navigate the frontend to a target route.
///
/// The command emits `nav:<target>` so the React tree (which owns the
/// router) can call `useNavigate()` on the matching path. Today we
/// recognise `"settings"` and `"recording"`; unknown targets return an
/// error rather than silently no-op, so caller bugs surface in the
/// `invoke` rejection rather than as a missing UI transition.
pub fn open_window_event_name(target: &str) -> Result<&'static str, String> {
    match target {
        "settings" => Ok(EVENT_NAV_SETTINGS),
        "recording" => Ok(EVENT_NAV_RECORDING),
        other => Err(format!("unknown navigation target: {other}")),
    }
}

/// Pick the newest `*.md` file directly under `vault_root` and return
/// its basename minus the `.md` extension.
///
/// Returns `Ok(None)` when:
/// - `vault_root` is empty / not a directory, or
/// - the directory has no `*.md` children.
///
/// Failures during `read_dir` / `metadata` for an individual entry
/// are skipped (best-effort) rather than aborting — the tray's
/// "Open last note" should not error out because of one unreadable
/// file. Unrecoverable IO at the directory level surfaces as `Err`.
pub(crate) fn newest_note_basename(
    vault_root: &std::path::Path,
) -> std::io::Result<Option<String>> {
    if vault_root.as_os_str().is_empty() || !vault_root.is_dir() {
        return Ok(None);
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(vault_root)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".md") else {
            continue;
        };
        // Skip hidden / empty stems — `.md` alone or `.foo.md` are
        // not valid heron note ids and would route the frontend to a
        // `/review/` URL with a leading dot that breaks the slug.
        if stem.is_empty() || stem.starts_with('.') {
            continue;
        }
        let Ok(modified) = meta.modified().or_else(|_| meta.created()) else {
            continue;
        };
        match best {
            Some((ref best_time, _)) if *best_time >= modified => {}
            _ => best = Some((modified, stem.to_owned())),
        }
    }
    Ok(best.map(|(_, stem)| stem))
}

/// Tauri command: return the newest note's basename (no extension), or
/// `None` if the vault is empty / unset. Reads `Settings.vault_root`
/// from the platform-default settings.json — wired so the renderer
/// can drive the same lookup the tray uses without re-deriving the
/// vault path on the JS side.
#[tauri::command]
pub fn heron_last_note_session_id() -> Result<Option<String>, String> {
    let settings_path = crate::default_settings_path();
    let settings = crate::read_settings(&settings_path).map_err(|e| e.to_string())?;
    if settings.vault_root.is_empty() {
        return Ok(None);
    }
    let vault = std::path::Path::new(&settings.vault_root);
    newest_note_basename(vault).map_err(|e| e.to_string())
}

/// Pick the newest note in the user's vault and either:
/// - emit `nav:review` with the session id payload (frontend
///   navigates to `/review/<id>`), or
/// - emit `nav:no_last_note` so the React tree pops a "no notes yet"
///   toast.
///
/// Settled on a frontend-toast over a true system notification so we
/// avoid pulling in `tauri-plugin-notification` for a single string —
/// the tray click already focuses the main window, so the user sees
/// the toast immediately.
fn open_last_note_dispatch<R: Runtime>(app: &AppHandle<R>) {
    // Always focus the main window so the user sees the toast or
    // navigates to a route — kept up-front so every code path below
    // shares the same UX (no "tray click did nothing" tail).
    focus_main_window(app);

    let settings_path = crate::default_settings_path();
    let settings = match crate::read_settings(&settings_path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("[heron-tray] open last note: failed to read settings: {err}");
            emit_no_last_note(app);
            return;
        }
    };

    if settings.vault_root.is_empty() {
        emit_no_last_note(app);
        return;
    }

    let vault = std::path::Path::new(&settings.vault_root);
    let newest = match newest_note_basename(vault) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("[heron-tray] walking vault failed: {err}");
            emit_no_last_note(app);
            return;
        }
    };

    let Some(session_id) = newest else {
        emit_no_last_note(app);
        return;
    };

    // The payload is a typed object so the JS listener can
    // destructure `payload.sessionId` without parsing a string.
    // Empty payload would force the listener to re-do the lookup on
    // every tray click.
    //
    // `tauri::Emitter::emit` requires `Serialize + Clone`; an owned
    // `String` carries the cheapest `Clone` impl (single allocation,
    // refcount-free) without forcing a bespoke wrapper.
    #[derive(serde::Serialize, Clone)]
    struct ReviewPayload {
        #[serde(rename = "sessionId")]
        session_id: String,
    }
    let payload = ReviewPayload { session_id };
    if let Err(e) = app.emit(EVENT_NAV_REVIEW, payload) {
        eprintln!("[heron-tray] failed to emit {EVENT_NAV_REVIEW}: {e}");
    }
}

/// Emit the `nav:no_last_note` event so the React tree pops a Sonner
/// toast. Logs (without panicking) on emit failure so the tray's
/// caller doesn't have to repeat the boilerplate.
fn emit_no_last_note<R: Runtime>(app: &AppHandle<R>) {
    if let Err(e) = app.emit(EVENT_NO_LAST_NOTE, ()) {
        eprintln!("[heron-tray] failed to emit {EVENT_NO_LAST_NOTE}: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn visual_maps_pre_recording_states_to_idle() {
        // The "armed" + "armed_cooldown" states sit between the
        // consent gate and the actual capture — they should look idle
        // in the menubar so the user doesn't think recording started
        // before they confirmed.
        for state in [
            RecordingState::Idle,
            RecordingState::Armed,
            RecordingState::ArmedCooldown,
        ] {
            assert_eq!(
                TrayVisual::from_state(state),
                TrayVisual::Idle,
                "state {state:?} should map to Idle"
            );
        }
    }

    #[test]
    fn visual_maps_capture_phase_states_one_to_one() {
        assert_eq!(
            TrayVisual::from_state(RecordingState::Recording),
            TrayVisual::Recording,
        );
        assert_eq!(
            TrayVisual::from_state(RecordingState::Transcribing),
            TrayVisual::Transcribing,
        );
        assert_eq!(
            TrayVisual::from_state(RecordingState::Summarizing),
            TrayVisual::Summarizing,
        );
    }

    #[test]
    fn every_visual_carries_an_icon_and_tooltip() {
        for visual in [
            TrayVisual::Idle,
            TrayVisual::Recording,
            TrayVisual::Transcribing,
            TrayVisual::Summarizing,
            TrayVisual::Error,
        ] {
            assert!(!visual.icon_bytes().is_empty(), "{visual:?} icon empty");
            assert!(!visual.tooltip().is_empty(), "{visual:?} tooltip empty");
        }
    }

    #[test]
    fn open_window_event_name_recognises_known_targets() {
        assert_eq!(open_window_event_name("settings"), Ok(EVENT_NAV_SETTINGS));
        assert_eq!(open_window_event_name("recording"), Ok(EVENT_NAV_RECORDING));
    }

    #[test]
    fn open_window_event_name_rejects_unknown_target() {
        // `Result::err` turns the `Ok` arm into `None`, so a missing
        // error explicitly fails the test rather than silently
        // unwrapping. Avoids the workspace-wide ban on `expect()`.
        let Err(err) = open_window_event_name("review") else {
            panic!("review is not a registered nav target");
        };
        assert!(
            err.contains("review"),
            "error should mention the target: {err}"
        );
    }

    #[test]
    fn newest_note_basename_returns_none_when_vault_root_is_empty_string() {
        let p = std::path::Path::new("");
        let out = newest_note_basename(p).expect("call");
        assert!(out.is_none());
    }

    #[test]
    fn newest_note_basename_returns_none_when_no_md_files() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        std::fs::write(tmp.path().join("not-a-note.txt"), b"x").expect("write");
        let out = newest_note_basename(tmp.path()).expect("call");
        assert!(out.is_none());
    }

    #[test]
    fn newest_note_basename_returns_none_when_path_does_not_exist() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let phantom = tmp.path().join("does-not-exist");
        let out = newest_note_basename(&phantom).expect("call");
        assert!(out.is_none());
    }

    #[test]
    fn newest_note_basename_picks_most_recent_md_file() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let older = tmp.path().join("2024-01-01-meeting.md");
        let newer = tmp.path().join("2026-04-25-meeting.md");
        std::fs::write(&older, b"older").expect("older");
        // Sleep long enough to clear typical FS mtime resolution
        // (HFS+ is ~1 s; APFS is sub-second but rounds; ext4 + xfs
        // depend on `noatime` mount opts). 50 ms covers APFS in
        // practice and keeps the suite under 1 s.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&newer, b"newer").expect("newer");

        let out = newest_note_basename(tmp.path())
            .expect("call")
            .expect("should pick a note");
        assert_eq!(out, "2026-04-25-meeting");
    }

    #[test]
    fn newest_note_basename_skips_dotfile_md_and_extensionless_files() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        std::fs::write(tmp.path().join(".hidden.md"), b"x").expect("hidden");
        std::fs::write(tmp.path().join("README"), b"x").expect("readme");
        std::fs::write(tmp.path().join("notes.md"), b"x").expect("notes");
        let out = newest_note_basename(tmp.path())
            .expect("call")
            .expect("notes.md should win");
        assert_eq!(out, "notes");
    }

    #[test]
    fn newest_note_basename_strips_md_extension_only() {
        // A file named `foo.bar.md` has stem `foo.bar`. We strip
        // exactly the `.md` suffix — the stem is the slug the
        // VaultWriter wrote, hyphens / dots inside it are part of
        // the session id.
        let tmp = tempfile::TempDir::new().expect("tmp");
        std::fs::write(tmp.path().join("foo.bar.md"), b"x").expect("write");
        let out = newest_note_basename(tmp.path())
            .expect("call")
            .expect("should pick");
        assert_eq!(out, "foo.bar");
    }
}
