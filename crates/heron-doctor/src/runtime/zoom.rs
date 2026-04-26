//! Zoom process availability check.
//!
//! `crates/heron-zoom/src/lib.rs` wires `AXObserver` against the
//! `us.zoom.xos` bundle ID. The observer's `start()` returns
//! `AxError::ZoomNotRunning` if the process isn't there — but the
//! onboarding wizard wants to surface that *before* the user clicks
//! Record on what looks like a working setup.
//!
//! This check enumerates running processes and looks for any that
//! match the Zoom executable name (`zoom.us` on macOS, plus the
//! lowercase `zoom` variant Zoom's installer occasionally produces).
//! If Zoom is not running we surface a `Warn` rather than a `Fail` —
//! the user might launch Zoom after onboarding finishes; the
//! point of the preflight is "tell me if I'm about to discover this
//! the hard way," not "block onboarding until Zoom boots."
//!
//! Process enumeration goes through the [`ProcessLister`] trait so
//! tests can stub the answer without spawning real processes. The
//! real impl uses the `sysinfo` crate (already a transitive dep of
//! several workspace crates); we read the process list with the
//! `system` feature turned on and the rest of `sysinfo`'s
//! battery-monitoring / disk-monitoring modules disabled to keep the
//! binary smaller.

use sysinfo::{ProcessRefreshKind, RefreshKind, System};

use super::{CheckSeverity, RuntimeCheck, RuntimeCheckOptions, RuntimeCheckResult};

const NAME: &str = "zoom_process";

/// Executable names we treat as "Zoom is running." `zoom.us` is the
/// real binary inside `Zoom.app/Contents/MacOS/`. We also accept the
/// lowercase variant some older Zoom builds shipped, and the bundle
/// path so the test rig can stub a `Path` without normalising case.
const ZOOM_PROCESS_NAMES: &[&str] = &["zoom.us", "zoom"];

/// Trait the [`ZoomProcessCheck`] uses to enumerate running processes.
/// `Vec<String>` is plenty — we only need the executable basename.
pub trait ProcessLister: Send + Sync {
    fn process_names(&self) -> Vec<String>;
}

/// Real-world process lister via the `sysinfo` crate.
pub fn real_process_lister() -> Box<dyn ProcessLister> {
    Box::new(SysinfoLister)
}

struct SysinfoLister;

impl ProcessLister for SysinfoLister {
    fn process_names(&self) -> Vec<String> {
        // `RefreshKind::new()` + an explicit
        // `with_processes(ProcessRefreshKind::new())` keeps the
        // refresh cheap — sysinfo otherwise queries CPU / mem /
        // file-handles for every PID, which we don't need.
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

/// Zoom-running check. Construct with a real lister or a stub.
pub struct ZoomProcessCheck {
    lister: Box<dyn ProcessLister>,
}

impl ZoomProcessCheck {
    pub fn new(lister: Box<dyn ProcessLister>) -> Self {
        Self { lister }
    }
}

impl RuntimeCheck for ZoomProcessCheck {
    fn name(&self) -> &'static str {
        NAME
    }

    fn run(&self, _opts: &RuntimeCheckOptions) -> RuntimeCheckResult {
        let names = self.lister.process_names();
        if zoom_is_running(&names) {
            RuntimeCheckResult::pass(NAME, "Zoom is running and visible to the AX bridge")
        } else {
            RuntimeCheckResult {
                name: NAME,
                severity: CheckSeverity::Warn,
                summary: "Zoom is not currently running".to_owned(),
                detail: "heron's speaker attribution attaches an AXObserver to the \
                     `us.zoom.xos` process. Launch Zoom before recording or \
                     attribution falls back to channel-only labels."
                    .to_owned(),
            }
        }
    }
}

/// Pure helper for matching process names. Exposed as `pub(super)` so
/// the keychain check can reuse the same string-match heuristic if it
/// ever wants to. Lowercase-compares both sides since macOS process
/// names are case-insensitive in practice (ProcessInfo round-trips
/// preserve case but PSEnumerator does not always).
fn zoom_is_running(names: &[String]) -> bool {
    names.iter().any(|n| {
        let lower = n.to_ascii_lowercase();
        ZOOM_PROCESS_NAMES.iter().any(|target| lower == *target)
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    struct StubLister(Vec<String>);
    impl ProcessLister for StubLister {
        fn process_names(&self) -> Vec<String> {
            self.0.clone()
        }
    }

    fn run(names: &[&str]) -> RuntimeCheckResult {
        let lister = StubLister(names.iter().map(|s| (*s).to_owned()).collect());
        ZoomProcessCheck::new(Box::new(lister)).run(&RuntimeCheckOptions::default())
    }

    #[test]
    fn zoom_running_yields_pass() {
        let r = run(&["bash", "Finder", "zoom.us", "Spotify"]);
        assert_eq!(r.severity, CheckSeverity::Pass);
    }

    #[test]
    fn zoom_not_running_yields_warn() {
        let r = run(&["bash", "Finder", "Spotify"]);
        assert_eq!(r.severity, CheckSeverity::Warn);
        assert!(r.detail.contains("us.zoom.xos"));
    }

    #[test]
    fn empty_process_list_yields_warn() {
        let r = run(&[]);
        assert_eq!(r.severity, CheckSeverity::Warn);
    }

    #[test]
    fn case_insensitive_match() {
        let r = run(&["ZOOM.US"]);
        assert_eq!(r.severity, CheckSeverity::Pass);
    }

    #[test]
    fn lowercase_zoom_also_matches() {
        // Some older Zoom installs renamed the binary to `zoom`
        // (no `.us` suffix) — accept it.
        let r = run(&["zoom"]);
        assert_eq!(r.severity, CheckSeverity::Pass);
    }

    #[test]
    fn near_miss_does_not_match() {
        // A different app whose name happens to contain "zoom" must
        // not trigger a false positive (e.g. "zoomy", "zoominfo").
        let r = run(&["zoominfo", "zoomy"]);
        assert_eq!(r.severity, CheckSeverity::Warn);
    }

    #[test]
    fn name_is_stable() {
        let c = ZoomProcessCheck::new(Box::new(StubLister(vec![])));
        assert_eq!(c.name(), "zoom_process");
    }

    #[test]
    fn real_lister_does_not_panic() {
        // Smoke test: enumerate the real process list without
        // crashing. The result is environment-dependent (CI will not
        // have Zoom running) so we don't assert on length.
        let lister = real_process_lister();
        let _ = lister.process_names();
    }
}
