//! Tauri surface for `heron-doctor`'s runtime preflight checks (gap #6).
//!
//! `heron-doctor` ships [`heron_doctor::Doctor::run_runtime_checks`]:
//! one consolidated probe across ONNX runtime health, Zoom process
//! availability, keychain ACL (macOS), and network reachability. Until
//! this module landed, the React onboarding wizard called the four
//! per-step `heron_test_*` probes and never asked the doctor for the
//! consolidated "is this machine ready to record?" answer. The doctor
//! existed; nothing surfaced it.
//!
//! This module ships [`heron_run_runtime_checks`] — a `#[tauri::command]`
//! shim that spawns the doctor's probes off the Tauri event loop via
//! `spawn_blocking` and returns one [`RuntimeCheckEntry`] per check.
//! The check set blocks for up to ~3 s in the worst case (network
//! reachability with default deadlines), and Tauri's event loop is the
//! same thread the WebView pumps frames on — running blocking work
//! directly there freezes the wizard's spinner. `spawn_blocking` keeps
//! the loop responsive while the doctor does its work. The wire shape
//! is deliberately JSON-friendly (snake_case `severity`, stable check
//! names) so the renderer can switch on `name` without parsing free
//! text.
//!
//! ## Why a thin re-shape rather than re-exporting `RuntimeCheckResult`?
//!
//! [`heron_doctor::CheckSeverity`] is `#[non_exhaustive]`. A future
//! variant added to the doctor crate would otherwise serialize across
//! the IPC bridge as a fresh string the renderer's TS union doesn't
//! cover. [`RuntimeCheckEntry`] pins a closed `pass`/`warn`/`fail`
//! enum at this boundary; the `From<CheckSeverity>` impl buckets any
//! unknown variant as `warn` so the existing renderer keeps rendering
//! something coherent. When a new variant lands, that `From` arm is
//! the single migration point.

use heron_doctor::{CheckSeverity, Doctor, RuntimeCheckResult};
use serde::Serialize;

/// One runtime-check result, shaped for the Tauri IPC bridge.
///
/// Mirrors `heron_doctor::RuntimeCheckResult` but with owned strings
/// throughout so the value can be serialized + handed to the renderer
/// without `'static` constraints. Fields stay 1:1 with the doctor
/// struct; the renderer in `apps/desktop/src/lib/invoke.ts` keeps the
/// matching `RuntimeCheckEntry` interface in lockstep.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeCheckEntry {
    /// Stable identifier (`onnx_runtime`, `zoom_process`, `keychain_acl`,
    /// `network_reachability`) — matches `heron_doctor::RuntimeCheckResult::name`.
    /// The renderer switches on this to render per-check copy.
    pub name: String,
    /// `pass` / `warn` / `fail` — matches `CheckSeverity` after
    /// `#[serde(rename_all = "snake_case")]`.
    pub severity: Severity,
    /// Human-readable one-liner suitable for the wizard status row.
    pub summary: String,
    /// Verbose diagnostic the user can copy into a support ticket.
    /// Empty for `pass` results.
    pub detail: String,
}

/// Wire-format severity. Re-declared here (rather than re-exporting
/// `heron_doctor::CheckSeverity`) so the desktop-side wire shape is
/// stable independent of the doctor crate's `#[non_exhaustive]`
/// attribute, and so the JSON labels are pinned at this boundary.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Pass,
    Warn,
    Fail,
}

impl From<CheckSeverity> for Severity {
    fn from(s: CheckSeverity) -> Self {
        match s {
            CheckSeverity::Pass => Severity::Pass,
            CheckSeverity::Warn => Severity::Warn,
            CheckSeverity::Fail => Severity::Fail,
            // `CheckSeverity` is `#[non_exhaustive]`. A future variant
            // (the doctor's `Skipped` aspiration) bucketed as `warn`
            // keeps the wizard's visual contract — Next is enabled —
            // without needing a frontend release in lockstep with the
            // doctor crate. When that variant lands and the wizard
            // wants distinct copy, this match arm is the migration
            // point.
            _ => Severity::Warn,
        }
    }
}

impl From<RuntimeCheckResult> for RuntimeCheckEntry {
    fn from(r: RuntimeCheckResult) -> Self {
        Self {
            name: r.name.to_string(),
            severity: r.severity.into(),
            summary: r.summary,
            detail: r.detail,
        }
    }
}

/// Tauri command (gap #6 surfacing): run the consolidated runtime-check
/// set and return JSON-friendly results to the React onboarding
/// wizard.
///
/// Wrapped in `spawn_blocking` because:
///
/// - The default check set includes [`heron_doctor::NetworkReachabilityCheck`],
///   which performs blocking HTTPS reachability probes with a 3 s
///   per-target deadline. Running those on the Tauri event loop blocks
///   WebView frame pumping and freezes the wizard's spinner.
/// - The keychain probe (macOS) talks to Security.framework on the
///   calling thread, which is fine off the main thread but inadvisable
///   to run synchronously from an `async` Tauri command (it would hold
///   the runtime worker for the full keychain ACL lookup — milliseconds
///   on a healthy login keychain, seconds on a locked one).
///
/// Errors flatten to a single `Err(String)` because the renderer
/// surfaces the message inline ("Could not run runtime checks: …") —
/// it doesn't switch on the underlying `JoinError` shape. A panic in
/// the doctor itself would be unusual (its probes are designed to
/// return `Result`s, not panic), but we log it before flattening so
/// the daemon log carries the panic context — otherwise the renderer
/// shows a generic "task failed" string with no breadcrumb.
#[tauri::command]
pub async fn heron_run_runtime_checks() -> Result<Vec<RuntimeCheckEntry>, String> {
    tokio::task::spawn_blocking(|| {
        Doctor::run_runtime_checks()
            .into_iter()
            .map(RuntimeCheckEntry::from)
            .collect()
    })
    .await
    .map_err(|e| {
        if e.is_panic() {
            tracing::error!(error = %e, "heron-doctor runtime check panicked");
        }
        format!("runtime checks task failed: {e}")
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn severity_serializes_snake_case() {
        let s = serde_json::to_string(&Severity::Pass).expect("ser pass");
        assert_eq!(s, "\"pass\"");
        let s = serde_json::to_string(&Severity::Warn).expect("ser warn");
        assert_eq!(s, "\"warn\"");
        let s = serde_json::to_string(&Severity::Fail).expect("ser fail");
        assert_eq!(s, "\"fail\"");
    }

    #[test]
    fn entry_round_trips_doctor_result() {
        let r = RuntimeCheckResult::fail("onnx_runtime", "missing", "no dylib found");
        let entry: RuntimeCheckEntry = r.into();
        assert_eq!(entry.name, "onnx_runtime");
        assert_eq!(entry.severity, Severity::Fail);
        assert_eq!(entry.summary, "missing");
        assert_eq!(entry.detail, "no dylib found");
    }

    #[test]
    fn entry_serializes_with_stable_keys() {
        let entry = RuntimeCheckEntry {
            name: "zoom_process".into(),
            severity: Severity::Warn,
            summary: "Zoom not running".into(),
            detail: "Open Zoom before recording".into(),
        };
        let s = serde_json::to_string(&entry).expect("ser entry");
        assert!(s.contains(r#""name":"zoom_process""#));
        assert!(s.contains(r#""severity":"warn""#));
        assert!(s.contains(r#""summary":"Zoom not running""#));
        assert!(
            s.contains(r#""detail":"Open Zoom before recording""#),
            "actual: {s}"
        );
    }

    #[test]
    fn pass_entry_keeps_empty_detail() {
        let r = RuntimeCheckResult::pass("network_reachability", "all targets reachable");
        let entry: RuntimeCheckEntry = r.into();
        assert_eq!(entry.severity, Severity::Pass);
        assert_eq!(entry.detail, "");
    }

    #[test]
    fn check_severity_round_trips_each_known_variant() {
        // Pin the bridge between `heron_doctor::CheckSeverity` and our
        // closed `Severity` enum on every known variant. A doctor-side
        // rename (e.g. `Pass` → `Ok`) breaks the renderer's TS union
        // silently because `RuntimeCheckSeverity` never sees the new
        // string — this test surfaces the mismatch at compile time
        // (variant added) or test time (variant renamed).
        assert_eq!(Severity::from(CheckSeverity::Pass), Severity::Pass);
        assert_eq!(Severity::from(CheckSeverity::Warn), Severity::Warn);
        assert_eq!(Severity::from(CheckSeverity::Fail), Severity::Fail);
    }

    #[test]
    fn vec_of_results_serializes_for_the_renderer() {
        // Integration of the conversion path the Tauri command takes:
        // `Vec<RuntimeCheckResult>` → `Vec<RuntimeCheckEntry>` → JSON.
        // Pins the mixed-severity wire shape so a doctor-side field
        // rename (`summary` → `message`) breaks here, not silently in
        // the wizard.
        let results = vec![
            RuntimeCheckResult::pass("onnx_runtime", "models present"),
            RuntimeCheckResult::warn(
                "zoom_process",
                "Zoom not running",
                "Open Zoom before recording",
            ),
            RuntimeCheckResult::fail("network_reachability", "offline", "no DNS"),
        ];
        let entries: Vec<RuntimeCheckEntry> =
            results.into_iter().map(RuntimeCheckEntry::from).collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "onnx_runtime");
        assert_eq!(entries[0].severity, Severity::Pass);
        assert_eq!(entries[1].severity, Severity::Warn);
        assert_eq!(entries[1].detail, "Open Zoom before recording");
        assert_eq!(entries[2].severity, Severity::Fail);

        let json = serde_json::to_string(&entries).expect("ser vec");
        // Field names + snake_case severity labels are the renderer's
        // contract — pin them on the serialised form, not just the
        // struct.
        assert!(json.contains(r#""severity":"pass""#));
        assert!(json.contains(r#""severity":"warn""#));
        assert!(json.contains(r#""severity":"fail""#));
        assert!(json.contains(r#""summary":"Zoom not running""#));
    }
}
