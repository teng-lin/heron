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
use tauri::tray::{TrayIcon, TrayIconBuilder, TrayIconId};
use tauri::{AppHandle, Emitter, Manager, Runtime, image::Image};
use tauri_plugin_notification::NotificationExt;
use tokio::time::interval;

use heron_types::RecordingState;

/// Stable id for the menubar tray. `TrayIconBuilder::with_id` registers
/// the tray under this label so [`heron_emit_capture_degraded`] (and a
/// future FSM dispatch path) can look it up via
/// [`AppHandle::tray_by_id`] without holding a `TrayIcon` clone in
/// global state. Const because there is exactly one tray for the
/// lifetime of the app.
pub const TRAY_ID: &str = "heron-main-tray";

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
/// Phase 75 (PR-ν): copy for the native macOS notification fired when
/// the tray's "Open last note…" runs and the vault is empty / unset.
/// Centralised here so the unit test can pin the exact wire shape and
/// the frontend can mirror the message in any future fallback path
/// without re-deriving it.
///
/// We dropped the previous `nav:no_last_note` event + Sonner toast in
/// favour of a real macOS notification (Notification Center entry,
/// banner, badge): the previous toast disappeared with the focused
/// window and was easy to miss when the tray click landed on a
/// background app.
///
/// **Hotkey hard-coded.** The body names `⌘⇧R` literally rather than
/// reading `Settings::record_hotkey`, matching the PR-ν brief. A user
/// who has remapped the chord still gets a useful action ("start a
/// recording from the tray") and the heron app surfaces the active
/// hotkey in Settings → Hotkey. A future PR can swap the literal for
/// a formatted version of the user's bound chord without breaking the
/// public surface — both constants stay `pub` for that path.
pub const NOTIFY_NO_LAST_NOTE_TITLE: &str = "heron";
pub const NOTIFY_NO_LAST_NOTE_BODY: &str =
    "No notes yet \u{2014} start a recording from the tray or hit \u{2318}\u{21E7}R.";
/// Phase 73 (PR-λ): contextual error event the frontend renders as a
/// Sonner toast with a "View diagnostics" action button (per the
/// `plan.md` week-12 line "Tap lost Zoom at 00:42:15 — transcript may
/// have gaps in that window").
///
/// Real wiring lands when the FSM's `CaptureDegraded` event is
/// integrated into the recording pipeline; today the surface is
/// reachable only through the [`heron_emit_capture_degraded`] manual-
/// fire command so the tray + toast UX can be polished independently
/// of the audio pipeline.
pub const EVENT_TRAY_DEGRADED: &str = "tray:degraded";

/// Discriminator for the `tray:degraded` payload's `kind` field. The
/// three variants cover the failure modes documented in
/// `docs/plan.md` week 12: tap-lost (process tap couldn't follow the
/// target app), AX-unavailable (Accessibility went away mid-call), and
/// AEC-overflow (echo canceller's input ringbuffer fell behind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradedKind {
    TapLost,
    AxUnavailable,
    AecOverflow,
}

impl DegradedKind {
    /// Parse the wire-format discriminant the frontend / tests send.
    /// Centralised so the command shim's error message is uniform.
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "tap_lost" => Ok(Self::TapLost),
            "ax_unavailable" => Ok(Self::AxUnavailable),
            "aec_overflow" => Ok(Self::AecOverflow),
            other => Err(format!(
                "unknown degraded kind: {other} (expected tap_lost / ax_unavailable / aec_overflow)",
            )),
        }
    }

    /// Human-facing copy for the tray tooltip on degraded state. The
    /// frontend's Sonner toast formats its own message from the full
    /// payload; this string is the tray-tooltip-only view, hence the
    /// "—" continuation matches the rest of the tooltip family.
    fn tooltip_suffix(self) -> &'static str {
        match self {
            Self::TapLost => "tap lost",
            Self::AxUnavailable => "AX unavailable",
            Self::AecOverflow => "AEC overflow",
        }
    }
}

/// Wire-format payload for `tray:degraded`. Field names match the
/// frontend's `DegradedPayload` interface in `lib/invoke.ts`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DegradedPayload {
    pub kind: DegradedKind,
    pub at_secs: u64,
    /// Optional target-app label (e.g. "Zoom") so the toast can render
    /// "Tap lost Zoom at 00:42:15" rather than the unattributed
    /// "Tap lost at 00:42:15".
    pub target: Option<String>,
    /// Active recording's session id when the FSM dispatches this
    /// (post-pipeline-integration). The frontend uses it as the
    /// navigation target for the toast's "View diagnostics" action.
    /// `None` → the toast falls back to "newest note in the vault",
    /// which is the only target available before the wiring PR lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

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
    /// Phase 73 (PR-λ): rendered when [`heron_emit_capture_degraded`]
    /// fires. The polling loop never produces this on its own — only
    /// the explicit degraded-event path flips the tray red.
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

    let tray = TrayIconBuilder::with_id(TrayIconId::new(TRAY_ID))
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
/// - fire a native macOS notification ("No notes yet …") via
///   [`notify_no_last_note`].
///
/// **Phase 75 (PR-ν) note.** The previous draft emitted a payload-less
/// `nav:no_last_note` event so `useTrayNav` could pop a Sonner toast.
/// That worked but tied the affordance to the focused window — a tray
/// click while a different app was foregrounded showed the toast
/// behind whatever the user was looking at. The macOS notification
/// surface is the right primitive: it lands in Notification Center
/// (so the user sees it on return), shows a banner regardless of
/// focus, and is exactly what `tauri-plugin-notification` exists for.
/// The fallback degrades gracefully when the user has not granted
/// notification permission (see [`notify_no_last_note`]).
fn open_last_note_dispatch<R: Runtime>(app: &AppHandle<R>) {
    // Always focus the main window so the user sees the navigation
    // outcome — the notification still fires whether or not the
    // window is foregrounded, but a focus-front-and-center matches
    // the tray click affordance.
    focus_main_window(app);

    let settings_path = crate::default_settings_path();
    let settings = match crate::read_settings(&settings_path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("[heron-tray] open last note: failed to read settings: {err}");
            notify_no_last_note(app);
            return;
        }
    };

    if settings.vault_root.is_empty() {
        notify_no_last_note(app);
        return;
    }

    let vault = std::path::Path::new(&settings.vault_root);
    let newest = match newest_note_basename(vault) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("[heron-tray] walking vault failed: {err}");
            notify_no_last_note(app);
            return;
        }
    };

    let Some(session_id) = newest else {
        notify_no_last_note(app);
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

/// Phase 75 (PR-ν): fire the "no notes yet" macOS notification.
///
/// Replaces the previous `nav:no_last_note` Sonner toast. The plugin
/// builder takes care of the platform plumbing (NSUserNotification on
/// macOS); we only need to set title + body and call `.show()`.
///
/// Permission handling. The Tauri 2 desktop plugin reports
/// `PermissionState::Granted` unconditionally — macOS handles the
/// "do not disturb" / per-app authorisation transparently inside
/// NSUserNotificationCenter, so the plugin doesn't need its own
/// gate. We still respect a `Denied` state defensively in case a
/// future plugin rev surfaces real macOS authorisation; on a
/// `Prompt` state we run `request_permission()` once before showing.
/// Either path that doesn't end in `Granted` short-circuits without
/// attempting `.show()` — the user already has the window-focus
/// affordance from the caller.
///
/// Every failure mode degrades to a no-op: the tray click has already
/// focused the main window, so the user is not left wondering whether
/// the click registered. Errors are logged to stderr for power-user
/// diagnosis and never propagated upward.
fn notify_no_last_note<R: Runtime>(app: &AppHandle<R>) {
    use tauri::plugin::PermissionState;

    let notification = app.notification();

    // Probe the current permission state. An error here is logged but
    // doesn't abort — a future plugin rev might restrict the probe to
    // a permission-gated capability, in which case we'd rather try
    // `.show()` and let it fail than refuse to surface anything.
    let state = match notification.permission_state() {
        Ok(state) => state,
        Err(err) => {
            eprintln!("[heron-tray] notify permission_state probe failed: {err}");
            // Fall through to .show(); the plugin may have its own
            // graceful "permission denied" path.
            PermissionState::Granted
        }
    };

    let granted = match state {
        PermissionState::Granted => true,
        PermissionState::Denied => false,
        // `Prompt` / `PromptWithRationale`: ask once. The desktop
        // plugin returns `Granted` from `request_permission` without
        // user interaction, so the prompt-then-show path is a single
        // cheap call; the macOS authorisation prompt itself comes
        // from AppKit when `.show()` runs.
        PermissionState::Prompt | PermissionState::PromptWithRationale => {
            match notification.request_permission() {
                Ok(PermissionState::Granted) => true,
                Ok(_) => false,
                Err(err) => {
                    eprintln!("[heron-tray] notify request_permission failed: {err}");
                    false
                }
            }
        }
    };

    if !granted {
        // Honour the user's explicit denial. Re-prompting on every
        // tray click would be hostile; the caller has already focused
        // the main window, which is sufficient feedback.
        return;
    }

    let result = notification
        .builder()
        .title(NOTIFY_NO_LAST_NOTE_TITLE)
        .body(NOTIFY_NO_LAST_NOTE_BODY)
        .show();
    if let Err(e) = result {
        // Notification permission may have been revoked between our
        // probe and the .show() (rare but possible), or the runtime
        // may be off-platform (Linux CI, no notification daemon).
        // Either way the tray click has already focused the window,
        // so we just log and move on.
        eprintln!("[heron-tray] notify no-last-note failed: {e}");
    }
}

/// Format a tray tooltip for a degraded-capture event. Pulled out of
/// [`heron_emit_capture_degraded`] so the unit test below can pin the
/// exact wire shape without spinning up a Tauri runtime.
///
/// Examples:
///   - `tap_lost` + `target = Some("Zoom")` →
///     `"heron — tap lost (Zoom) at 00:42:15"`
///   - `ax_unavailable` + `target = None` →
///     `"heron — AX unavailable at 00:00:09"`
pub(crate) fn format_degraded_tooltip(payload: &DegradedPayload) -> String {
    let suffix = payload.kind.tooltip_suffix();
    let stamp = format_hms(payload.at_secs);
    match &payload.target {
        Some(target) if !target.is_empty() => {
            format!("heron — {suffix} ({target}) at {stamp}")
        }
        _ => format!("heron — {suffix} at {stamp}"),
    }
}

/// Format an `at_secs` count as `HH:MM:SS`. Matches the brief's
/// example "00:42:15" and the existing review-UI convention so the
/// tray and toast read identically.
fn format_hms(at_secs: u64) -> String {
    let hours = at_secs / 3600;
    let minutes = (at_secs % 3600) / 60;
    let seconds = at_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// Tauri command (PR-λ): emit a `tray:degraded` event the frontend
/// renders as a Sonner toast, and flip the tray to its error variant
/// with a contextual tooltip.
///
/// The brief intentionally exposes this as a manual-fire surface so
/// the tray + toast UX can be polished before the audio pipeline
/// lands its real `CaptureDegraded` emission. Once the FSM dispatch
/// path is integrated, the recording orchestrator calls this command
/// verbatim — no further frontend or tray work is required.
///
/// **Known limitation, intentional for this PR.** The tray's error
/// icon is sticky: the 1 s polling loop diffs against `last_visual`
/// (its own internal cache), and because the live FSM is always
/// `Idle` today the diff never fires and the error icon never gets
/// repainted away. The Sonner toast still auto-dismisses after 12 s,
/// so the dominant signal is transient; the menubar icon stays red
/// until a future FSM state change. The wiring PR is expected to
/// replace `spawn_status_poll` with an event-driven repaint hook
/// that knows about the degraded → idle transition, at which point
/// the icon will clear naturally.
///
/// Errors stringly so the JS caller (and a future Rust caller) can
/// surface a uniform message; the only error today is an unknown
/// `kind` discriminant.
#[tauri::command]
pub fn heron_emit_capture_degraded(
    app: tauri::AppHandle,
    kind: String,
    at_secs: u64,
    target: Option<String>,
    session_id: Option<String>,
) -> Result<(), String> {
    let parsed_kind = DegradedKind::from_str(&kind)?;
    let payload = DegradedPayload {
        kind: parsed_kind,
        at_secs,
        target,
        session_id,
    };
    flip_tray_to_error(&app, &payload);
    app.emit(EVENT_TRAY_DEGRADED, payload)
        .map_err(|e| e.to_string())
}

/// Look up the menubar tray by [`TRAY_ID`] and apply the error visual
/// plus a contextual tooltip. Best-effort: a missing tray (tests, or
/// a pre-`setup` window) is not fatal — the event still fires for the
/// frontend toast regardless.
///
/// **Race note for future readers.** The 1 s polling loop in
/// [`spawn_status_poll`] only repaints when it observes a changed
/// `TrayVisual`; because the current FSM never produces `Error` (and
/// the poll never reads our error icon), the loop's diff check skips
/// the repaint and our error visual sticks. When the FSM gains real
/// `CaptureDegraded` emission, the polling path and this flip path
/// will need to coordinate (most likely by replacing the poll with an
/// event-bus subscriber).
fn flip_tray_to_error<R: Runtime>(app: &AppHandle<R>, payload: &DegradedPayload) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        // First-paint races, tests without a tray, or a future
        // off-macOS build that omits the tray will fall through here.
        // The emitter still pushes the event so the frontend toast
        // fires regardless.
        return;
    };
    match Image::from_bytes(TrayVisual::Error.icon_bytes()) {
        Ok(image) => {
            if let Err(err) = tray.set_icon(Some(image)) {
                eprintln!("[heron-tray] degraded set_icon failed: {err}");
            }
        }
        Err(err) => {
            eprintln!("[heron-tray] degraded decode icon failed: {err}");
        }
    }
    let tooltip = format_degraded_tooltip(payload);
    if let Err(err) = tray.set_tooltip(Some(tooltip.as_str())) {
        eprintln!("[heron-tray] degraded set_tooltip failed: {err}");
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
    fn degraded_kind_round_trips_known_strings() {
        // The frontend emits these literal strings via
        // `invoke("heron_emit_capture_degraded", { kind: "..." })`.
        // Pinning the round-trip catches a future serde rename that
        // would silently drop the toast.
        for s in ["tap_lost", "ax_unavailable", "aec_overflow"] {
            let parsed = DegradedKind::from_str(s).expect("known kind");
            let serialised = serde_json::to_string(&parsed).expect("serialise");
            assert_eq!(serialised, format!("\"{s}\""));
        }
    }

    #[test]
    fn degraded_kind_rejects_unknown_string() {
        let err = DegradedKind::from_str("bogus_kind").expect_err("unknown");
        assert!(err.contains("bogus_kind"));
    }

    #[test]
    fn format_degraded_tooltip_with_target_renders_target_in_parens() {
        // Brief: "Tap lost Zoom at 00:42:15 — transcript may have gaps".
        // The tray tooltip is a shorter form ("heron — tap lost (Zoom)
        // at 00:42:15"); the React toast renders the long sentence.
        let payload = DegradedPayload {
            kind: DegradedKind::TapLost,
            at_secs: 42 * 60 + 15,
            target: Some("Zoom".to_owned()),
            session_id: None,
        };
        assert_eq!(
            format_degraded_tooltip(&payload),
            "heron — tap lost (Zoom) at 00:42:15",
        );
    }

    #[test]
    fn format_degraded_tooltip_without_target_omits_parens() {
        // No target — the tooltip falls back to a clean "kind at HMS"
        // shape rather than rendering "() at ...".
        let payload = DegradedPayload {
            kind: DegradedKind::AxUnavailable,
            at_secs: 9,
            target: None,
            session_id: None,
        };
        assert_eq!(
            format_degraded_tooltip(&payload),
            "heron — AX unavailable at 00:00:09",
        );
    }

    #[test]
    fn format_degraded_tooltip_treats_empty_target_as_missing() {
        // A frontend bug that passes `target: ""` shouldn't render
        // "(empty parens) at 00:01:00" — the empty-string branch
        // collapses to the no-target form.
        let payload = DegradedPayload {
            kind: DegradedKind::AecOverflow,
            at_secs: 60,
            target: Some(String::new()),
            session_id: None,
        };
        assert_eq!(
            format_degraded_tooltip(&payload),
            "heron — AEC overflow at 00:01:00",
        );
    }

    #[test]
    fn degraded_payload_serialises_with_optional_session_id() {
        // Pin both shapes the wiring PR will produce: today's
        // manual-fire path (no session id; field omitted via
        // `skip_serializing_if`) and tomorrow's FSM-emit path (active
        // recording's id present). The frontend's listener depends on
        // `session_id` being undefined when absent so the
        // `fallbackToNewest` branch in `App.tsx` fires.
        let no_id = DegradedPayload {
            kind: DegradedKind::TapLost,
            at_secs: 5,
            target: Some("Zoom".to_owned()),
            session_id: None,
        };
        let s = serde_json::to_string(&no_id).expect("serialise");
        // No `session_id` key when the value is `None` — keeps the
        // wire shape compact and lets the frontend's `?? null`
        // pattern observe undefined.
        assert!(!s.contains("session_id"), "got: {s}");

        let with_id = DegradedPayload {
            kind: DegradedKind::TapLost,
            at_secs: 5,
            target: Some("Zoom".to_owned()),
            session_id: Some("abc-123".to_owned()),
        };
        let s = serde_json::to_string(&with_id).expect("serialise");
        assert!(s.contains(r#""session_id":"abc-123""#), "got: {s}");
    }

    #[test]
    fn format_hms_rolls_over_at_hour_boundary() {
        // Plain integer division — pin the rollover so a future
        // refactor doesn't accidentally produce "01:60:00" for one
        // hour.
        assert_eq!(format_hms(0), "00:00:00");
        assert_eq!(format_hms(59), "00:00:59");
        assert_eq!(format_hms(60), "00:01:00");
        assert_eq!(format_hms(3_600), "01:00:00");
        assert_eq!(format_hms(3_661), "01:01:01");
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

    #[test]
    fn no_last_note_notification_copy_pinned() {
        // The phase 75 brief specifies the user-facing strings; pin
        // them so a future copy edit goes through code review rather
        // than slipping in unnoticed. The keyboard chord uses the
        // U+2318 (PLACE OF INTEREST SIGN, "Command") and U+21E7 (UPWARDS
        // WHITE ARROW, "Shift") code points so the notification renders
        // the real Mac glyphs in Notification Center.
        assert_eq!(NOTIFY_NO_LAST_NOTE_TITLE, "heron");
        assert_eq!(
            NOTIFY_NO_LAST_NOTE_BODY,
            "No notes yet — start a recording from the tray or hit \u{2318}\u{21E7}R.",
        );
        // Belt-and-braces: the body must contain the Mac chord glyphs
        // verbatim, not the ASCII fallback ("Cmd+Shift+R") that an
        // accidental copy edit might introduce.
        assert!(NOTIFY_NO_LAST_NOTE_BODY.contains('\u{2318}'));
        assert!(NOTIFY_NO_LAST_NOTE_BODY.contains('\u{21E7}'));
    }
}
