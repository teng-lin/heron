//! Runtime preflight checks (gap #6).
//!
//! The original `heron-doctor` is an **offline** post-mortem tool —
//! it reads `~/Library/Logs/heron/<date>.log` and flags anomalies
//! against thresholds. That's the right shape for "did last week's
//! sessions look healthy?" but it's the wrong shape for the question
//! the §9 onboarding wizard wants to answer:
//!
//! > Before the user records their first meeting, does this machine
//! > have everything heron needs to *succeed*?
//!
//! The four runtime checks below are the minimum set the onboarding
//! wizard needs to call so a fresh install fails on first run instead
//! of failing silently mid-meeting:
//!
//! | Check                       | Why                                                      |
//! |-----------------------------|----------------------------------------------------------|
//! | [`OnnxRuntimeCheck`]        | `docs/plan.md` §8 promises sherpa-onnx is the bundled    |
//! |                             | fallback; if the dylib doesn't load there's no transcript|
//! |                             | path at all.                                             |
//! | [`ZoomProcessCheck`]        | `heron-zoom`'s AXObserver wires against `us.zoom.xos`    |
//! |                             | (`crates/heron-zoom/src/lib.rs:39`). Without Zoom        |
//! |                             | running speaker attribution silently degrades.           |
//! | [`KeychainAclCheck`]        | `docs/security.md` §3.3 requires bundle-ID ACLs on the   |
//! |                             | API-key entries — without it any signed app can read.    |
//! | [`NetworkReachabilityCheck`]| Whisper model download + LLM summarize call both go      |
//! |                             | over HTTPS; preflight catches a captive-portal wifi      |
//! |                             | before the user records 30 minutes of audio.             |
//!
//! Each check is a standalone unit implementing [`RuntimeCheck`]; the
//! module-level [`run_all`] runs every check sequentially and returns
//! one [`RuntimeCheckResult`] per check. The wizard will eventually
//! call [`crate::Doctor::run_runtime_checks`] from
//! `apps/desktop/src-tauri/src/onboarding.rs` (gap #7, separate
//! scope); this module just exposes the API surface.
//!
//! ## Design notes
//!
//! - **No async.** Checks are short, blocking, and run on the wizard's
//!   button-click path; the orchestrator doesn't need an async
//!   reactor for a 1.5s preflight.
//! - **Severity is pass/warn/fail.** "Warn" lets us surface "Zoom
//!   isn't running but you can launch it later" without blocking the
//!   wizard from advancing. "Fail" is the things the user truly can't
//!   record without (no ONNX runtime, no network at all).
//! - **Mockable for tests.** Each check accepts an "environment" via
//!   trait/closure rather than reading the real world directly. Tests
//!   exercise the failure path with stub envs; the default ctor wires
//!   the real-world impls.
//! - **No dep on [`crate::log_reader`] / [`crate::anomalies`].** The
//!   offline checks and runtime checks share nothing; cross-pollution
//!   would force the onboarding wizard to depend on the JSONL parser
//!   it doesn't need.

use std::time::Duration;

use serde::Serialize;

#[cfg(target_os = "macos")]
mod keychain_macos;
mod network;
mod onnx;
mod zoom;

pub use network::{
    NetworkProbe, NetworkReachabilityCheck, ReachabilityTarget, default_targets,
    real_probe as real_network_probe,
};
pub use onnx::{OnnxProbe, OnnxRuntimeCheck, real_probe as real_onnx_probe};
pub use zoom::{ProcessLister, ZoomProcessCheck, real_process_lister};

#[cfg(target_os = "macos")]
pub use keychain_macos::{KeychainAclCheck, KeychainProbe, real_probe as real_keychain_probe};

/// Severity of a single runtime check result.
///
/// Mirrors the three states the onboarding wizard renders: a green
/// check (`Pass`), a yellow warning (`Warn`) the user can ignore, and
/// a red blocker (`Fail`) that prevents Next from being clickable.
///
/// `#[non_exhaustive]` so a future `Skipped` variant (for checks the
/// wizard deliberately bypasses) is non-breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CheckSeverity {
    /// Everything required is present and working.
    Pass,
    /// Something is degraded but recording can still proceed. Surface
    /// to the user; don't block.
    Warn,
    /// Recording cannot proceed. Block Next in the wizard, render the
    /// `summary` + `detail` so the user can fix the underlying issue.
    Fail,
}

/// Single check result.
///
/// `summary` is a one-liner suitable for the wizard's status row;
/// `detail` is the full diagnostic string the user can copy into a
/// support ticket. Both are plain text — no Markdown — so the same
/// struct round-trips through `serde_json` for the eventual
/// `heron-doctor runtime --json` shape.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeCheckResult {
    /// Stable identifier (`onnx_runtime`, `zoom_process`, etc.) so the
    /// wizard / a CI hook can match on a specific check without parsing
    /// `summary`.
    pub name: &'static str,
    pub severity: CheckSeverity,
    /// Human-readable one-liner. Renderer caps to ~80 cols.
    pub summary: String,
    /// Optional verbose diagnostic. Empty for pass results.
    pub detail: String,
}

impl RuntimeCheckResult {
    pub fn pass(name: &'static str, summary: impl Into<String>) -> Self {
        Self {
            name,
            severity: CheckSeverity::Pass,
            summary: summary.into(),
            detail: String::new(),
        }
    }

    pub fn warn(name: &'static str, summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name,
            severity: CheckSeverity::Warn,
            summary: summary.into(),
            detail: detail.into(),
        }
    }

    pub fn fail(name: &'static str, summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name,
            severity: CheckSeverity::Fail,
            summary: summary.into(),
            detail: detail.into(),
        }
    }
}

/// One runtime preflight check.
///
/// Implementations hold any environment / probe handles they need
/// (process lister, HTTP client, ONNX runtime probe) and run a single
/// blocking step in [`Self::run`]. Object-safe so the orchestrator
/// can hold a heterogeneous `Vec<Box<dyn RuntimeCheck>>` and run them
/// in order.
pub trait RuntimeCheck: Send + Sync {
    /// Stable identifier used in [`RuntimeCheckResult::name`].
    fn name(&self) -> &'static str;

    /// Run the check. Implementations must not panic and must respect
    /// the [`RuntimeCheckOptions::deadline`] when called via
    /// [`run_all_with_options`].
    fn run(&self, opts: &RuntimeCheckOptions) -> RuntimeCheckResult;
}

/// Per-check tunables. Fed through every [`RuntimeCheck::run`] call
/// so a future test rig can shorten timeouts without re-instantiating
/// the check structs.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeCheckOptions {
    /// Per-check timeout for blocking probes (network reachability,
    /// process listing). Defaults to [`Self::DEFAULT_DEADLINE`].
    pub deadline: Duration,
}

impl RuntimeCheckOptions {
    /// 3 s is short enough that the onboarding wizard renders within
    /// one frame even when every check times out, and long enough
    /// that a slow corp wifi handshake doesn't false-positive.
    pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(3);
}

impl Default for RuntimeCheckOptions {
    fn default() -> Self {
        Self {
            deadline: Self::DEFAULT_DEADLINE,
        }
    }
}

/// Run a fixed list of checks with the supplied options. Returns one
/// result per check, in input order. Never panics.
pub fn run_all(checks: &[Box<dyn RuntimeCheck>]) -> Vec<RuntimeCheckResult> {
    run_all_with_options(checks, &RuntimeCheckOptions::default())
}

/// Same as [`run_all`] but with caller-supplied options.
pub fn run_all_with_options(
    checks: &[Box<dyn RuntimeCheck>],
    opts: &RuntimeCheckOptions,
) -> Vec<RuntimeCheckResult> {
    checks.iter().map(|c| c.run(opts)).collect()
}

/// Default check set — the four checks gap #6 enumerated, wired with
/// real-world probes. The onboarding wizard calls this. Tests
/// instantiate the individual structs with mock probes instead.
///
/// Order matters: ONNX first (cheapest, no network), then Zoom
/// (cheap, local), then keychain (macOS-only / no-op elsewhere), then
/// network (slowest, may time out).
// `vec_init_then_push` would normally rewrite this to a `vec![]`
// literal, but the `#[cfg(target_os = "macos")]` arm only conditionally
// pushes — `vec![]` doesn't accept conditional elements, and splitting
// per-platform `vec!` literals duplicates the non-conditional checks.
#[allow(clippy::vec_init_then_push)]
pub fn default_checks() -> Vec<Box<dyn RuntimeCheck>> {
    let mut checks: Vec<Box<dyn RuntimeCheck>> = Vec::with_capacity(4);
    checks.push(Box::new(OnnxRuntimeCheck::new(real_onnx_probe())));
    checks.push(Box::new(ZoomProcessCheck::new(real_process_lister())));
    #[cfg(target_os = "macos")]
    checks.push(Box::new(KeychainAclCheck::new(real_keychain_probe())));
    checks.push(Box::new(NetworkReachabilityCheck::new(
        real_network_probe(),
        default_targets(),
    )));
    checks
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    struct StubCheck {
        result: RuntimeCheckResult,
    }

    impl RuntimeCheck for StubCheck {
        fn name(&self) -> &'static str {
            self.result.name
        }
        fn run(&self, _opts: &RuntimeCheckOptions) -> RuntimeCheckResult {
            self.result.clone()
        }
    }

    #[test]
    fn run_all_preserves_order_and_count() {
        let checks: Vec<Box<dyn RuntimeCheck>> = vec![
            Box::new(StubCheck {
                result: RuntimeCheckResult::pass("a", "ok"),
            }),
            Box::new(StubCheck {
                result: RuntimeCheckResult::fail("b", "bad", "details"),
            }),
            Box::new(StubCheck {
                result: RuntimeCheckResult::warn("c", "iffy", "details"),
            }),
        ];
        let out = run_all(&checks);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].name, "a");
        assert_eq!(out[1].severity, CheckSeverity::Fail);
        assert_eq!(out[2].severity, CheckSeverity::Warn);
    }

    #[test]
    fn result_serializes_with_severity_string() {
        let r = RuntimeCheckResult::fail("x", "summary", "detail");
        let s = serde_json::to_string(&r).expect("ser");
        assert!(s.contains(r#""severity":"fail""#));
        assert!(s.contains(r#""name":"x""#));
        assert!(s.contains(r#""summary":"summary""#));
    }

    #[test]
    fn pass_result_has_empty_detail() {
        let r = RuntimeCheckResult::pass("x", "all good");
        assert_eq!(r.detail, "");
    }

    #[test]
    fn default_options_deadline_is_three_seconds() {
        let opts = RuntimeCheckOptions::default();
        assert_eq!(opts.deadline, Duration::from_secs(3));
    }

    #[test]
    fn default_checks_includes_minimum_set() {
        let checks = default_checks();
        let names: Vec<&str> = checks.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"onnx_runtime"));
        assert!(names.contains(&"zoom_process"));
        assert!(names.contains(&"network_reachability"));
        // Keychain check is macOS-only.
        #[cfg(target_os = "macos")]
        assert!(names.contains(&"keychain_acl"));
    }
}
