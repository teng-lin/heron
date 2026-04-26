//! `heron-doctor` — diagnosis surfaces for heron.
//!
//! Two complementary surfaces:
//!
//! - **Offline log parsing.** Reads
//!   `~/Library/Logs/heron/<YYYY-MM-DD>.log` (one daily JSONL file
//!   per `docs/archives/observability.md`), parses each `kind:
//!   "session_summary"` record, and reports anomalies against the v1
//!   ship-criteria thresholds in `docs/archives/implementation.md` §18.2.
//!   Pure offline: no network, no model loads, no auth. The §15.4
//!   diagnostics tab consumes a single session's record; this is the
//!   *cross-session* counterpart for the user (and for the eventual
//!   `heron-doctor` automation hook in §16). API: [`detect_anomalies`].
//!
//! - **Runtime preflight checks** (gap #6 — added phase 83). Live
//!   probes the onboarding wizard runs on the user's machine before
//!   the first record: ONNX runtime health, Zoom availability,
//!   keychain ACL, network reachability. API: [`Doctor::run_runtime_checks`].
//!   The wizard wiring itself lives behind gap #7; this crate just
//!   ships the API surface.

pub mod anomalies;
pub mod log_reader;
pub mod runtime;

pub use anomalies::{Anomaly, AnomalyKind, Thresholds, detect_anomalies};
pub use log_reader::{
    LogReadError, MAX_LINE_LEN, SessionSummaryFields, SessionSummaryRecord, count_unknown_versions,
    read_session_summaries,
};
pub use runtime::{
    CheckSeverity, NetworkProbe, NetworkReachabilityCheck, OnnxProbe, OnnxRuntimeCheck,
    ProcessLister, ReachabilityTarget, RuntimeCheck, RuntimeCheckOptions, RuntimeCheckResult,
    ZoomProcessCheck, default_checks, default_targets, run_all, run_all_with_options,
};

#[cfg(target_os = "macos")]
pub use runtime::{KeychainAclCheck, KeychainProbe};

/// Top-level façade.
///
/// Currently a thin wrapper that exposes [`Self::run_runtime_checks`]
/// — the API the onboarding wizard (gap #7) calls before letting the
/// user advance to "record a meeting." Kept as a struct so a future
/// `Doctor::with_options(...)` constructor can layer on without a
/// breaking signature change.
pub struct Doctor;

impl Doctor {
    /// Run the default runtime-check set. Returns one
    /// [`RuntimeCheckResult`] per check, in deterministic order
    /// matching [`runtime::default_checks`].
    ///
    /// Never panics. Each check is bounded by
    /// [`RuntimeCheckOptions::DEFAULT_DEADLINE`] (3 s) for any
    /// blocking probe.
    pub fn run_runtime_checks() -> Vec<RuntimeCheckResult> {
        let checks = runtime::default_checks();
        runtime::run_all(&checks)
    }

    /// Same as [`Self::run_runtime_checks`] but with caller-supplied
    /// options. Lets onboarding tighten the deadline for snappier UX
    /// or loosen it on a known-slow corp wifi.
    pub fn run_runtime_checks_with(opts: &RuntimeCheckOptions) -> Vec<RuntimeCheckResult> {
        let checks = runtime::default_checks();
        runtime::run_all_with_options(&checks, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test for the public façade. Marked `#[ignore]` because
    /// `Doctor::run_runtime_checks` wires the **real** probes —
    /// running it during `cargo test` would touch the user's login
    /// keychain, hit live network targets, and enumerate every
    /// process on the box, none of which belongs in a hermetic test.
    /// Run on demand with
    /// `cargo test -p heron-doctor doctor_run_runtime_checks_is_callable -- --ignored`.
    #[test]
    #[ignore]
    fn doctor_run_runtime_checks_is_callable() {
        let results = Doctor::run_runtime_checks();
        assert!(!results.is_empty());
        for r in &results {
            assert!(!r.name.is_empty(), "every result must carry a name");
        }
    }
}
