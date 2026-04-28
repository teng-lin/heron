//! Auto-detect which meeting platform is currently running.
//!
//! The Home page's "Start recording" button has no implicit platform
//! context — the user could be on a Zoom call, a Google Meet, a
//! Teams call, or recording mic-only notes. The daemon's
//! `POST /v1/meetings` requires a [`Platform`] so the orchestrator
//! knows which `target_bundle_id` to attach the macOS process tap to
//! (see [`crate::lib`] / `heron-orchestrator/src/lib.rs:769`).
//!
//! This module enumerates running processes via `sysinfo` and matches
//! their executable names against a per-platform allowlist. The first
//! match (in deterministic priority order: Zoom → Google Meet →
//! Teams → Webex) wins. If nothing matches, the caller falls back to
//! [`Platform::Zoom`] — a slight conceptual lie when there's no
//! Zoom process, but the actual capture path still records the mic
//! into `mic.wav`; only the `tap.wav` system-audio sidecar comes up
//! silent.
//!
//! ## Why a separate module from `heron-doctor`
//!
//! `heron-doctor::runtime::zoom` does the same kind of scan, but for
//! a different purpose (preflight pass/warn surfaced in the
//! onboarding wizard). Reusing its `ProcessLister` trait would mean
//! either widening doctor's public API to expose more bundle-name
//! tables, or routing this detection through a runtime-check abstraction
//! that returns severity levels — neither shape fits a one-shot
//! "which Platform should I pick" query. The detection logic here is
//! ~30 LoC and only the desktop shell consumes it, so keeping it
//! local avoids cross-crate churn.
//!
//! Process enumeration goes through the [`ProcessLister`] trait so
//! tests can stub the answer without spawning real processes —
//! mirroring the same pattern `heron-doctor::runtime::zoom` uses.

use heron_session::Platform;
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

/// Trait the detector uses to enumerate running processes. `Vec<String>`
/// is plenty — we only need executable basenames to match against the
/// per-platform tables below.
pub trait ProcessLister: Send + Sync {
    fn process_names(&self) -> Vec<String>;
}

/// Real-world process lister via `sysinfo`. Same configuration
/// `heron-doctor`'s Zoom check uses (cheapest refresh kind that still
/// gives us names).
pub fn real_process_lister() -> Box<dyn ProcessLister> {
    Box::new(SysinfoLister)
}

struct SysinfoLister;

impl ProcessLister for SysinfoLister {
    fn process_names(&self) -> Vec<String> {
        let mut sys = System::new_with_specifics(
            RefreshKind::new().with_processes(ProcessRefreshKind::new()),
        );
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        sys.processes()
            .values()
            .map(|p| p.name().to_string_lossy().into_owned())
            .collect()
    }
}

/// Executable-name allowlists per [`Platform`]. macOS process names
/// are case-insensitive in practice, so we lowercase both sides at
/// match time.
///
/// **Zoom** — `zoom.us` is the binary inside `Zoom.app/Contents/MacOS/`;
/// older builds occasionally shipped a lowercase `zoom` variant
/// (mirrors `heron-doctor::runtime::zoom::ZOOM_PROCESS_NAMES`).
///
/// **Microsoft Teams** — `MSTeams` is the v2 Electron exec name;
/// older `Teams` covers v1 / Teams classic which some orgs still pin
/// to.
///
/// **Webex** — the modern Webex client's exec name. The legacy
/// `Webex Teams` variant uses the same string, so one entry covers
/// both.
///
/// **Google Meet** — runs inside the user's browser. We match the
/// four macOS Chromium-family browsers Meet supports (Chrome, Edge,
/// Brave, Arc) by their canonical exec names. Listed *last* on
/// purpose: "browser is open" is an extremely weak signal for "the
/// call is in Meet" — Chrome being open is the steady state on most
/// dev laptops. Without this ordering, a Teams call on a machine
/// that also has Chrome up would route the process tap to Chrome's
/// bundle id (`com.google.Chrome`, per
/// `heron-orchestrator/src/lib.rs:769`) and miss Teams audio
/// entirely. Sorting Meet to the bottom means we only fall to it
/// when no native client is running — which is exactly when Meet is
/// the most plausible call surface.
const PROCESS_NAMES: &[(Platform, &[&str])] = &[
    (Platform::Zoom, &["zoom.us", "zoom"]),
    (Platform::MicrosoftTeams, &["msteams", "teams"]),
    (Platform::Webex, &["webex"]),
    (
        Platform::GoogleMeet,
        &["google chrome", "microsoft edge", "brave browser", "arc"],
    ),
];

/// Detect the first running meeting platform from the priority order
/// in [`PROCESS_NAMES`]. Returns `None` when nothing matches.
///
/// Ordering matters: a single laptop can have Chrome AND Zoom running
/// simultaneously (browser parked in the background, Zoom in the
/// foreground for the actual call). Picking Zoom first handles that
/// common case correctly. Google Meet is intentionally last —
/// "Chrome is open" is too weak a signal to outrank a native call
/// client; see [`PROCESS_NAMES`] for the full rationale.
pub fn detect_meeting_platform(lister: &dyn ProcessLister) -> Option<Platform> {
    let names: Vec<String> = lister
        .process_names()
        .into_iter()
        .map(|n| n.to_ascii_lowercase())
        .collect();
    PROCESS_NAMES.iter().find_map(|(platform, allowlist)| {
        if names
            .iter()
            .any(|n| allowlist.iter().any(|target| n == target))
        {
            Some(*platform)
        } else {
            None
        }
    })
}

/// Tauri command — wire form mirrors `Option<Platform>`. The
/// frontend uses the result as a hint for `heron_start_capture`;
/// when `null`, the renderer falls back to [`Platform::Zoom`] so the
/// FSM still gets a valid `target_bundle_id` and `mic.wav` records
/// even if no meeting app is open.
///
/// `sysinfo`'s `refresh_processes(All)` does a `proc_listallpids` +
/// per-pid `proc_pidinfo` walk; on a typical macOS system that's
/// 5–50ms with a few hundred processes — not sub-millisecond. We
/// hop onto Tokio's blocking pool so we don't park the IPC worker
/// for that window; the `spawn_blocking` round-trip is ~10µs, well
/// below the work it isolates.
#[tauri::command]
pub async fn heron_detect_meeting_platform() -> Option<Platform> {
    tokio::task::spawn_blocking(|| {
        let lister = real_process_lister();
        detect_meeting_platform(lister.as_ref())
    })
    .await
    .unwrap_or(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubLister(Vec<String>);
    impl ProcessLister for StubLister {
        fn process_names(&self) -> Vec<String> {
            self.0.clone()
        }
    }

    fn detect_with(names: &[&str]) -> Option<Platform> {
        let lister = StubLister(names.iter().map(|s| (*s).to_owned()).collect());
        detect_meeting_platform(&lister)
    }

    #[test]
    fn detects_zoom() {
        assert_eq!(
            detect_with(&["bash", "Finder", "zoom.us", "Spotify"]),
            Some(Platform::Zoom)
        );
    }

    #[test]
    fn detects_zoom_lowercase_variant() {
        assert_eq!(
            detect_with(&["bash", "ZOOM", "Finder"]),
            Some(Platform::Zoom)
        );
    }

    #[test]
    fn detects_google_meet_via_chrome() {
        assert_eq!(
            detect_with(&["Finder", "Google Chrome", "Slack"]),
            Some(Platform::GoogleMeet)
        );
    }

    #[test]
    fn detects_teams_v2() {
        assert_eq!(
            detect_with(&["MSTeams", "Finder"]),
            Some(Platform::MicrosoftTeams)
        );
    }

    #[test]
    fn detects_teams_classic() {
        assert_eq!(
            detect_with(&["Teams", "Finder"]),
            Some(Platform::MicrosoftTeams)
        );
    }

    #[test]
    fn detects_webex() {
        assert_eq!(detect_with(&["webex", "Finder"]), Some(Platform::Webex));
    }

    #[test]
    fn priority_zoom_over_chrome() {
        // Both running — Zoom wins because it sits earlier in
        // PROCESS_NAMES. Reflects the common case where the user
        // parked Chrome in the background but is on a Zoom call.
        assert_eq!(
            detect_with(&["Google Chrome", "zoom.us"]),
            Some(Platform::Zoom)
        );
    }

    #[test]
    fn priority_teams_over_chrome() {
        // Native Teams client beats a "Chrome is open" Meet signal —
        // "browser running" is the steady state on dev laptops, so
        // Meet auto-detection only fires when no native call client
        // is up. Without this ordering, a Teams call on a Chrome-
        // always-open machine would route the tap to Chrome and
        // miss the Teams audio.
        assert_eq!(
            detect_with(&["MSTeams", "Google Chrome"]),
            Some(Platform::MicrosoftTeams)
        );
    }

    #[test]
    fn priority_webex_over_chrome() {
        // Same rationale as `priority_teams_over_chrome` — a native
        // Webex client outranks Chrome for the same reason.
        assert_eq!(
            detect_with(&["webex", "Google Chrome"]),
            Some(Platform::Webex)
        );
    }

    #[test]
    fn google_meet_only_when_no_native_client() {
        // Only when nothing else matches does Chrome get picked —
        // that's when Meet is the most plausible call surface.
        assert_eq!(
            detect_with(&["Finder", "Google Chrome"]),
            Some(Platform::GoogleMeet)
        );
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(detect_with(&["bash", "Finder", "Spotify"]), None);
    }

    #[test]
    fn empty_process_list_returns_none() {
        assert_eq!(detect_with(&[]), None);
    }

    #[test]
    fn case_insensitive_match() {
        assert_eq!(detect_with(&["ZOOM.US"]), Some(Platform::Zoom));
        assert_eq!(detect_with(&["MICROSOFT EDGE"]), Some(Platform::GoogleMeet));
    }
}
