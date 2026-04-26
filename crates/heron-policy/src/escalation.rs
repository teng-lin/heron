//! Escalation hook — the side-channel a [`crate::SpeechController`]
//! invokes when [`crate::filter::evaluate`] returns
//! [`crate::PolicyDecision::Escalate`].
//!
//! The filter only signals *what* should happen
//! ([`crate::EscalationMode::Notify`] / [`crate::EscalationMode::LeaveMeeting`]);
//! the hook is the seam where that intent turns into a real
//! side-effect (HTTP webhook, push notification, vault note, leave the
//! meeting, …). Splitting it out as a trait keeps `heron-policy` free
//! of HTTP/transport dependencies and lets each call site (desktop
//! shell, server, integration test) wire the appropriate handler.
//!
//! Per [`docs/archives/api-design-spec.md`](../../../docs/archives/api-design-spec.md) §9
//! the controller still emits the same
//! [`crate::SpeechEvent::Cancelled`] +
//! [`crate::CancelReason::PolicyDenied`] for an escalated utterance —
//! the hook fires *in addition* to that, not instead of it. The audit
//! log thus carries one Cancelled event per blocked utterance whether
//! or not an escalation was configured.
//!
//! ## Default in production
//!
//! [`DefaultSpeechController::new`](crate::DefaultSpeechController::new)
//! installs a [`LoggingEscalationHook`]. It writes a `tracing::warn!`
//! per escalation, which is the right floor for v1: the operator sees
//! the event in their daemon log even before any user-facing transport
//! is wired up. Production deployments swap in a richer hook via
//! [`DefaultSpeechController::with_escalation_hook`](crate::DefaultSpeechController::with_escalation_hook).

use async_trait::async_trait;

use crate::EscalationMode;

/// Side-effect a [`crate::SpeechController`] performs when the policy
/// filter requests escalation. Called *after* the controller has
/// emitted the `SpeechEvent::Cancelled { reason: PolicyDenied }` for
/// the blocked utterance, so the audit-log entry exists regardless of
/// whether the hook succeeds or fails.
///
/// Implementations should be cheap and non-blocking on the happy path
/// — the controller awaits this on the speak hot path. Long-running
/// work (HTTP retries, writing to disk) belongs behind a `tokio::spawn`
/// inside the hook itself, not in the await chain.
#[async_trait]
pub trait EscalationHook: Send + Sync {
    /// Fired when the filter returns
    /// [`crate::PolicyDecision::Escalate`]. `rule` is the matched
    /// `deny_topic:<term>` string for the audit log; `via` is the
    /// configured [`EscalationMode`] (always
    /// [`EscalationMode::Notify`] or [`EscalationMode::LeaveMeeting`]
    /// — `EscalationMode::None` collapses to plain `Denied` upstream
    /// and never reaches this hook).
    async fn escalate(&self, rule: String, via: EscalationMode);
}

/// The default hook installed by
/// [`crate::DefaultSpeechController::new`]. Logs the escalation via
/// `tracing::warn!` and returns. Suitable for v1, where no transport
/// is wired yet — production deployments override with a richer hook.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoggingEscalationHook;

#[async_trait]
impl EscalationHook for LoggingEscalationHook {
    async fn escalate(&self, rule: String, via: EscalationMode) {
        // `tracing::warn!` because an escalation always corresponds
        // to a denied utterance — the operator should see it without
        // needing to crank log levels.
        //
        // We log the *discriminant* of `via`, not the full struct,
        // so a `Notify { destination }` carrying a webhook URL or
        // tokenized email never lands in the warn stream. Production
        // hooks that need the destination should consume `via`
        // directly; the default hook is a fallback diagnostic and
        // should not leak routing material into logs.
        tracing::warn!(
            rule = %rule,
            via = via_kind(&via),
            "policy escalation fired (no transport wired; see EscalationHook impl)",
        );
    }
}

/// Discriminant-only label for [`EscalationMode`] safe to emit into
/// logs. Avoids leaking `Notify { destination }` (which may carry a
/// webhook URL or tokenized email).
fn via_kind(via: &EscalationMode) -> &'static str {
    match via {
        EscalationMode::None => "none",
        EscalationMode::Notify { .. } => "notify",
        EscalationMode::LeaveMeeting => "leave_meeting",
    }
}

/// Test-only fixture. Records every call so a test can assert on
/// the rule + mode that fired without standing up a tracing
/// subscriber. Not exported from the crate (only used by the
/// in-tree integration tests).
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct RecordingEscalationHook {
    inner: std::sync::Mutex<Vec<(String, EscalationMode)>>,
}

#[cfg(test)]
impl RecordingEscalationHook {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    pub(crate) fn calls(&self) -> Vec<(String, EscalationMode)> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg(test)]
#[async_trait]
impl EscalationHook for RecordingEscalationHook {
    async fn escalate(&self, rule: String, via: EscalationMode) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((rule, via));
    }
}
