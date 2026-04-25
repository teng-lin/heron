//! macOS menubar tray icon for the heron desktop shell.
//!
//! Phase 64 (PR-β) adds a status-bar tray that mirrors the recording
//! FSM (`heron_types::RecordingState`) and exposes a small dropdown
//! menu for the most-used affordances:
//!
//!   - **Open last note…** — placeholder, logs to stdout for now;
//!     wired up once the vault writer lands a `last_note` accessor.
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
            // Placeholder — the vault writer doesn't expose a
            // "last note" cursor yet. Logging makes the tray click
            // visible during local dev without crashing the process.
            println!("[heron-tray] Open last note: not yet wired (phase 64 follow-up).");
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

#[cfg(test)]
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
}
