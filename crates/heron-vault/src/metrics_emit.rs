//! Metrics for `heron-vault` write paths.
//!
//! Per #225 acceptance: `vault_write_duration_seconds{op}` histogram +
//! `vault_failures_total{op, reason}` counter, both flowing through
//! the shared [`heron_metrics::timed_io_sync`] helper. The `op`
//! dimension is bounded to a closed enum (`atomic_write`,
//! `update_action_item`, `finalize`), and the `reason` dimension on
//! failures is enum-shaped via [`heron_metrics::ClassifyFailure`].
//!
//! Privacy: every label flows through [`heron_metrics::redacted!`]
//! string literals — there is no `format!` of paths, IDs, or note
//! content into a label dimension. Reviewers verifying this can grep
//! the crate for `redacted!` and confirm every call uses a literal.

use heron_metrics::{ClassifyFailure, RedactedLabel, redacted};

use crate::writer::VaultError;

pub(crate) const VAULT_WRITE_DURATION_SECONDS: &str = "vault_write_duration_seconds";
/// Public so `heron-orchestrator::vault_read` can reuse the canonical
/// read-side duration metric without re-declaring it.
pub const VAULT_READ_DURATION_SECONDS: &str = "vault_read_duration_seconds";
/// Unified failures counter (#239). Read and write paths share a
/// single \`vault_failures_total\{op, reason}\` series so dashboards
/// see "any vault op failed" in one place; the \`op\` label
/// distinguishes \`atomic_write\` / \`update_action_item\` / \`finalize\`
/// (and future read-side ops once read-path instrumentation lands).
/// Public so the orchestrator's \`vault_read\` can emit on the same
/// series. Replaces the prior \`vault_write_failures_total\` and
/// \`vault_read_failures_total\` split.
pub const VAULT_FAILURES_TOTAL: &str = "vault_failures_total";

/// Pinned `op` labels for vault write metrics. Closed set keeps
/// cardinality at the {atomic_write, update_action_item, finalize}
/// vertex — adding a new write path means landing a corresponding
/// label here, and PR reviewers can verify the addition is bounded.
pub(crate) fn op_atomic_write() -> RedactedLabel {
    redacted!("atomic_write")
}

pub(crate) fn op_update_action_item() -> RedactedLabel {
    redacted!("update_action_item")
}

pub(crate) fn op_finalize() -> RedactedLabel {
    redacted!("finalize")
}

pub(crate) fn op_re_summarize() -> RedactedLabel {
    redacted!("re_summarize")
}

impl ClassifyFailure for VaultError {
    fn failure_reason(&self) -> RedactedLabel {
        match self {
            Self::Io(e) => match e.kind() {
                std::io::ErrorKind::NotFound => redacted!("io_not_found"),
                std::io::ErrorKind::PermissionDenied => redacted!("io_permission_denied"),
                std::io::ErrorKind::AlreadyExists => redacted!("io_already_exists"),
                _ => redacted!("io_error"),
            },
            Self::Yaml(_) => redacted!("yaml_error"),
            Self::MissingFrontmatter { .. } => redacted!("missing_frontmatter"),
            Self::UnterminatedFrontmatter { .. } => redacted!("unterminated_frontmatter"),
            Self::ActionItemNotFound { .. } => redacted!("action_item_not_found"),
        }
    }
}

/// `std::io::Error` is the bare error returned by [`crate::writer::atomic_write`].
/// The [`ClassifyFailure`] trait can't blanket-impl over the orphan
/// type, so this newtype wraps an `io::Error` for the sync helper's
/// `E: ClassifyFailure` bound and unwraps cleanly back into
/// `io::Result` at the call site.
pub(crate) struct IoErrorReason(pub std::io::Error);

impl ClassifyFailure for IoErrorReason {
    fn failure_reason(&self) -> RedactedLabel {
        match self.0.kind() {
            std::io::ErrorKind::NotFound => redacted!("io_not_found"),
            std::io::ErrorKind::PermissionDenied => redacted!("io_permission_denied"),
            std::io::ErrorKind::AlreadyExists => redacted!("io_already_exists"),
            _ => redacted!("io_error"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_metrics::validate_metric_name;

    #[test]
    fn vault_metric_names_match_convention() {
        for name in [
            VAULT_WRITE_DURATION_SECONDS,
            VAULT_READ_DURATION_SECONDS,
            VAULT_FAILURES_TOTAL,
        ] {
            validate_metric_name(name)
                .unwrap_or_else(|e| panic!("metric name {name:?} drifted: {e}"));
        }
    }

    #[test]
    fn vault_error_classify_failure_covers_known_kinds() {
        let cases: &[(VaultError, &str)] = &[
            (
                VaultError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
                "io_not_found",
            ),
            (
                VaultError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                "io_permission_denied",
            ),
            (
                VaultError::Io(std::io::Error::from(std::io::ErrorKind::AlreadyExists)),
                "io_already_exists",
            ),
            (
                VaultError::Io(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
                "io_error",
            ),
            (
                VaultError::ActionItemNotFound {
                    id: heron_types::ItemId::from_u128(1),
                },
                "action_item_not_found",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.failure_reason().as_str(), *expected, "for {err:?}");
        }
    }
}
