//! Network reachability check.
//!
//! Heron makes exactly two outbound network classes (per
//! `docs/security.md` "What heron deliberately does NOT do"):
//!
//! 1. **Whisper / sherpa-onnx model download** — one-shot at first
//!    run, hits `github.com` (`crates/heron-speech/src/sherpa.rs:89`).
//! 2. **LLM summarize call** — per-session, hits the user's chosen
//!    backend (Anthropic by default, `https://api.anthropic.com` per
//!    `crates/heron-llm/src/anthropic.rs:47`).
//!
//! A captive-portal wifi or a corp firewall that blocks one of these
//! is the single most common "I tried heron and it broke" failure
//! mode for non-technical users — they record a 30-minute meeting and
//! discover at summarize-time that the LLM call is blocked. The
//! preflight check resolves "is the egress to those hosts open?"
//! before the user records anything.
//!
//! ## What we probe
//!
//! - `https://api.anthropic.com/` — HEAD request. The Anthropic
//!   service returns 401/405 for unauthenticated HEAD; either is
//!   "the host is reachable and TLS works", which is all the
//!   preflight asks. We don't auth the request.
//! - `https://github.com/` — used to fetch the sherpa-onnx model.
//!   GitHub HEAD returns 200 / 301 unauthenticated.
//!
//! Each target is probed sequentially with the supplied deadline as a
//! per-call timeout. A `Fail` if **all** probes fail (no network);
//! `Warn` if *some* fail (one backend is reachable, the user can
//! still summarize). `Pass` if every probe gets a TCP+TLS handshake.
//!
//! ## Why not just one connect()?
//!
//! A single connect to one host can pass while the actual API host
//! the user needs is firewalled. Two probes catches the common corp
//! "github allowlisted, OpenAI/Anthropic blocked" pattern at a few
//! hundred ms cost. We deliberately don't attempt every realtime LLM
//! provider's URL — the surface multiplies fast and the user's
//! chosen backend is already represented by Anthropic for v1.
//!
//! ## What this does NOT detect
//!
//! - **Captive portals that proxy 200 OK.** A hotel / airport portal
//!   that intercepts HTTPS and returns a redirect can defeat any
//!   unauthenticated check. We require HTTPS so a plain-HTTP portal
//!   fails the TLS handshake; defending against transparent
//!   middle-boxes would require pinning Anthropic's cert or sending
//!   an authenticated probe, both out of scope for v1.
//! - **DNS hijack to an attacker-controlled host.** Same shape — a
//!   reachable host that isn't actually api.anthropic.com would
//!   pass. Cert pinning would catch it; deferred.

use std::time::Duration;

use super::{CheckSeverity, RuntimeCheck, RuntimeCheckOptions, RuntimeCheckResult};

const NAME: &str = "network_reachability";

/// One reachability target. `purpose` is rendered into the failure
/// detail so the user knows what feature is at risk.
#[derive(Debug, Clone)]
pub struct ReachabilityTarget {
    pub url: String,
    pub purpose: String,
}

/// Default targets — what `default_checks()` wires.
///
/// Sourced from:
/// - `crates/heron-llm/src/anthropic.rs::DEFAULT_BASE_URL`
/// - `crates/heron-speech/src/sherpa.rs::SILERO_VAD_URL` (host-only)
pub fn default_targets() -> Vec<ReachabilityTarget> {
    vec![
        ReachabilityTarget {
            url: "https://api.anthropic.com/".to_owned(),
            purpose: "LLM summarizer (Anthropic API)".to_owned(),
        },
        ReachabilityTarget {
            url: "https://github.com/".to_owned(),
            purpose: "sherpa-onnx model download".to_owned(),
        },
    ]
}

/// Outcome of a single reachability probe.
///
/// `#[non_exhaustive]` so a future variant (e.g. `Slow { ms }` or
/// `CertExpired`) lands as non-breaking.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeOutcome {
    /// Host responded (any HTTP status counts as reachable — 401, 200,
    /// or 301 all mean TLS handshake + TCP connect succeeded).
    Reachable,
    /// Probe failed. `reason` is a short string suitable for the
    /// onboarding wizard's "details" panel.
    Unreachable { reason: String },
}

/// Probe trait. Real impl uses [`reqwest::blocking`]; tests stub.
pub trait NetworkProbe: Send + Sync {
    fn probe(&self, target: &ReachabilityTarget, timeout: Duration) -> ProbeOutcome;
}

/// Real-world probe via `reqwest::blocking`. Builds the HTTP client
/// once at construction so per-target probes don't pay the TLS-stack
/// init cost N times.
pub fn real_probe() -> Box<dyn NetworkProbe> {
    Box::new(ReqwestProbe::new())
}

struct ReqwestProbe {
    /// Pre-built blocking client. `None` only if construction failed
    /// at startup (very rare — `reqwest::blocking::Client::builder`
    /// fails on rustls-tls init issues, which would also break the
    /// summarizer path). We hold an `Option` rather than a `Result`
    /// so the per-call site is `match` not `?`-fallback, and the
    /// failure renders as a structured `Unreachable` per target.
    client: Option<reqwest::blocking::Client>,
}

impl ReqwestProbe {
    fn new() -> Self {
        // `connect_timeout` here is a hard ceiling; the per-call
        // `timeout` is set on the request below so it can shrink
        // when the orchestrator splits the deadline across N
        // targets.
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .ok();
        Self { client }
    }
}

impl NetworkProbe for ReqwestProbe {
    fn probe(&self, target: &ReachabilityTarget, timeout: Duration) -> ProbeOutcome {
        let client = match self.client.as_ref() {
            Some(c) => c,
            None => {
                return ProbeOutcome::Unreachable {
                    reason: "reqwest blocking client failed to initialise at startup".to_owned(),
                };
            }
        };
        match client.head(&target.url).timeout(timeout).send() {
            Ok(_) => ProbeOutcome::Reachable,
            Err(e) => ProbeOutcome::Unreachable {
                reason: e.to_string(),
            },
        }
    }
}

/// Network reachability check. Aggregates per-target probes into a
/// single result. Construct with a real probe + [`default_targets`]
/// or with a stub for tests.
pub struct NetworkReachabilityCheck {
    probe: Box<dyn NetworkProbe>,
    targets: Vec<ReachabilityTarget>,
}

impl NetworkReachabilityCheck {
    pub fn new(probe: Box<dyn NetworkProbe>, targets: Vec<ReachabilityTarget>) -> Self {
        Self { probe, targets }
    }
}

impl RuntimeCheck for NetworkReachabilityCheck {
    fn name(&self) -> &'static str {
        NAME
    }

    fn run(&self, opts: &RuntimeCheckOptions) -> RuntimeCheckResult {
        if self.targets.is_empty() {
            return RuntimeCheckResult::warn(
                NAME,
                "no reachability targets configured",
                "the doctor was constructed with an empty target list — \
                 re-instantiate via NetworkReachabilityCheck::new with \
                 default_targets()",
            );
        }

        // Split the per-check deadline across the N probes so the
        // total wall-time stays bounded by `opts.deadline` rather
        // than `N * opts.deadline`. Floor at 500 ms — anything
        // smaller starts to false-fail on a healthy-but-slow corp
        // wifi handshake.
        let total = self.targets.len();
        let per_probe_timeout = (opts.deadline / total as u32).max(Duration::from_millis(500));

        // We only need the failures + the pass-count for the summary,
        // so don't allocate a parallel Vec of pass names.
        let mut failures: Vec<(String, String)> = Vec::new();
        for target in &self.targets {
            if let ProbeOutcome::Unreachable { reason } =
                self.probe.probe(target, per_probe_timeout)
            {
                failures.push((target.purpose.clone(), reason));
            }
        }

        if failures.is_empty() {
            return RuntimeCheckResult::pass(
                NAME,
                format!("all {total} reachability target(s) responded"),
            );
        }
        let detail = failures
            .iter()
            .map(|(purpose, reason)| format!("• {purpose}: {reason}"))
            .collect::<Vec<_>>()
            .join("\n");
        let all_failed = failures.len() == total;
        let severity = if all_failed {
            CheckSeverity::Fail
        } else {
            CheckSeverity::Warn
        };
        let summary = if all_failed {
            "no upstream backends reachable — check wifi / firewall".to_owned()
        } else {
            format!("{} of {total} backend(s) unreachable", failures.len())
        };
        RuntimeCheckResult {
            name: NAME,
            severity,
            summary,
            detail,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Stub probe shared between the test body and the
    /// `NetworkReachabilityCheck` via `Arc` so the test can read the
    /// recorded timeouts after the run without unsafe casts. The
    /// `NetworkProbe` impl forwards to the inner state.
    struct SharedStub {
        inner: Arc<StubInner>,
    }

    struct StubInner {
        outcomes: Mutex<Vec<ProbeOutcome>>,
        timeouts: Mutex<Vec<Duration>>,
    }

    impl SharedStub {
        fn new(outcomes: Vec<ProbeOutcome>) -> (Self, Arc<StubInner>) {
            let inner = Arc::new(StubInner {
                outcomes: Mutex::new(outcomes),
                timeouts: Mutex::new(Vec::new()),
            });
            (
                Self {
                    inner: Arc::clone(&inner),
                },
                inner,
            )
        }
    }

    impl NetworkProbe for SharedStub {
        fn probe(&self, _target: &ReachabilityTarget, timeout: Duration) -> ProbeOutcome {
            self.inner
                .timeouts
                .lock()
                .expect("stub mutex poisoned")
                .push(timeout);
            // Pop in order so we can script per-target outcomes.
            self.inner
                .outcomes
                .lock()
                .expect("stub mutex poisoned")
                .remove(0)
        }
    }

    /// Convenience for the tests that don't care about recorded
    /// timeouts — preserves the original test signatures.
    struct StubProbe {
        outcomes: Mutex<Vec<ProbeOutcome>>,
    }

    impl StubProbe {
        fn new(outcomes: Vec<ProbeOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
            }
        }
    }

    impl NetworkProbe for StubProbe {
        fn probe(&self, _target: &ReachabilityTarget, _timeout: Duration) -> ProbeOutcome {
            self.outcomes.lock().expect("stub mutex poisoned").remove(0)
        }
    }

    fn target(purpose: &str) -> ReachabilityTarget {
        ReachabilityTarget {
            url: format!("https://example.com/{purpose}"),
            purpose: purpose.to_owned(),
        }
    }

    #[test]
    fn all_reachable_yields_pass() {
        let check = NetworkReachabilityCheck::new(
            Box::new(StubProbe::new(vec![
                ProbeOutcome::Reachable,
                ProbeOutcome::Reachable,
            ])),
            vec![target("a"), target("b")],
        );
        let r = check.run(&RuntimeCheckOptions::default());
        assert_eq!(r.severity, CheckSeverity::Pass);
        assert!(r.summary.contains("2"));
    }

    #[test]
    fn all_unreachable_yields_fail() {
        let check = NetworkReachabilityCheck::new(
            Box::new(StubProbe::new(vec![
                ProbeOutcome::Unreachable {
                    reason: "dns: no route".to_owned(),
                },
                ProbeOutcome::Unreachable {
                    reason: "tls handshake".to_owned(),
                },
            ])),
            vec![target("a"), target("b")],
        );
        let r = check.run(&RuntimeCheckOptions::default());
        assert_eq!(r.severity, CheckSeverity::Fail);
        // Detail should contain both purpose lines + reasons.
        assert!(r.detail.contains("• a: dns: no route"));
        assert!(r.detail.contains("• b: tls handshake"));
    }

    #[test]
    fn partial_reachable_yields_warn() {
        let check = NetworkReachabilityCheck::new(
            Box::new(StubProbe::new(vec![
                ProbeOutcome::Reachable,
                ProbeOutcome::Unreachable {
                    reason: "timeout".to_owned(),
                },
            ])),
            vec![target("anthropic"), target("github")],
        );
        let r = check.run(&RuntimeCheckOptions::default());
        assert_eq!(r.severity, CheckSeverity::Warn);
        assert!(r.summary.contains("1 of 2"));
        assert!(r.detail.contains("• github: timeout"));
        assert!(!r.detail.contains("• anthropic"));
    }

    #[test]
    fn empty_targets_yields_warn() {
        let check = NetworkReachabilityCheck::new(Box::new(StubProbe::new(vec![])), vec![]);
        let r = check.run(&RuntimeCheckOptions::default());
        assert_eq!(r.severity, CheckSeverity::Warn);
        assert!(r.summary.contains("no reachability targets"));
    }

    #[test]
    fn name_is_stable() {
        let check = NetworkReachabilityCheck::new(Box::new(StubProbe::new(vec![])), vec![]);
        assert_eq!(check.name(), "network_reachability");
    }

    #[test]
    fn default_targets_includes_anthropic_and_github() {
        let targets = default_targets();
        assert!(targets.iter().any(|t| t.url.contains("api.anthropic.com")));
        assert!(targets.iter().any(|t| t.url.contains("github.com")));
    }

    #[test]
    fn deadline_is_split_across_targets() {
        // With 3s deadline and 2 targets we expect ~1.5s per probe,
        // not 3s per probe (which would let the cumulative wall-time
        // hit 6s — see Gemini review note).
        let (stub, inner) = SharedStub::new(vec![ProbeOutcome::Reachable, ProbeOutcome::Reachable]);
        let opts = RuntimeCheckOptions {
            deadline: Duration::from_secs(3),
        };
        let check = NetworkReachabilityCheck::new(Box::new(stub), vec![target("a"), target("b")]);
        let _ = check.run(&opts);

        let timeouts = inner.timeouts.lock().expect("timeouts lock").clone();
        assert_eq!(timeouts.len(), 2);
        for t in &timeouts {
            assert!(
                *t <= Duration::from_secs(2),
                "per-probe timeout {t:?} should be ≤ deadline / N"
            );
            assert!(
                *t >= Duration::from_millis(500),
                "per-probe timeout {t:?} must respect the 500ms floor"
            );
        }
    }

    #[test]
    fn timeout_floor_kicks_in_for_many_targets() {
        // 6 targets × 1s deadline would naively floor at ~166 ms per
        // probe; the 500 ms floor should override.
        let (stub, inner) = SharedStub::new(vec![ProbeOutcome::Reachable; 6]);
        let opts = RuntimeCheckOptions {
            deadline: Duration::from_secs(1),
        };
        let targets: Vec<_> = (0..6).map(|i| target(&format!("t{i}"))).collect();
        let check = NetworkReachabilityCheck::new(Box::new(stub), targets);
        let _ = check.run(&opts);

        let timeouts = inner.timeouts.lock().expect("timeouts lock").clone();
        for t in &timeouts {
            assert!(
                *t >= Duration::from_millis(500),
                "{t:?} must respect 500ms floor"
            );
        }
    }
}
