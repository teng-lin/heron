//! Validation for [`crate::PolicyProfile`] before it is handed to a
//! [`crate::SpeechController`] for the lifetime of a session.
//!
//! [`crate::filter::evaluate`] is the *runtime* matcher; this module
//! is the *configuration-time* sanity check that catches the
//! mistakes which would otherwise silently corrupt every later
//! decision: empty topic strings (match nothing or everything
//! depending on substring semantics), duplicate entries (a profile
//! author who edits a config twice), and overlap between allow and
//! deny (where deny wins per [`crate::filter`] but the configuration
//! is almost certainly a mistake worth surfacing loudly).
//!
//! Pure / synchronous: callable on the orchestrator's session-start
//! path without a clock or thread hop, mirroring
//! `heron_realtime::validate` (phase 55).
//!
//! ## Invariants
//!
//! Duplicate / overlap comparison is **case-insensitive only** — no
//! `.trim()`. That mirrors [`crate::filter::evaluate`] exactly, which
//! lowercases both haystack and needle but treats whitespace as part
//! of the substring. A profile with `["pricing", " pricing "]`
//! validates because at runtime the two are genuinely different
//! matchers (one matches anywhere, the other requires whitespace
//! boundaries). The emptiness check below still uses
//! `.trim().is_empty()` so all-whitespace entries are rejected as
//! malformed config.
//!
//! Case folding uses Rust's [`str::to_lowercase`] (Unicode-default).
//! Locale-sensitive forms (Turkish dotted/dotless I) and combining-
//! character variants (NFC vs. NFD) are treated as distinct, matching
//! `filter::evaluate`. ASCII topic strings — the realistic case —
//! behave as expected.
//!
//! Failures are returned in priority order so a single misconfigured
//! profile produces a deterministic error message run-to-run:
//!
//! 1. Empty / whitespace-only `allow_topics` entry.
//! 2. Empty / whitespace-only `deny_topics` entry.
//! 3. Duplicate inside `allow_topics`.
//! 4. Duplicate inside `deny_topics`.
//! 5. Topic appears in both lists.
//! 6. `EscalationMode::Notify { destination }` with empty destination.
//!
//! `mute = true` is *not* a configuration error — it is a runtime
//! kill switch the controller honors per [`crate::filter::evaluate`].

use crate::{EscalationMode, PolicyProfile};

/// Reasons a [`PolicyProfile`] cannot be admitted to a session.
/// Distinct variants per failure so callers can branch on the cause
/// without parsing strings.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("allow_topics contains an empty or whitespace-only entry")]
    EmptyAllowTopic,
    #[error("deny_topics contains an empty or whitespace-only entry")]
    EmptyDenyTopic,
    #[error("allow_topics has duplicate entry: {topic:?}")]
    DuplicateAllowTopic { topic: String },
    #[error("deny_topics has duplicate entry: {topic:?}")]
    DuplicateDenyTopic { topic: String },
    #[error("topic {topic:?} appears in both allow_topics and deny_topics")]
    AllowDenyOverlap { topic: String },
    #[error("escalation = Notify but destination is empty or whitespace-only")]
    EmptyNotifyDestination,
}

/// Run all validations against `profile`. Returns `Ok(())` when the
/// profile is safe to install on a [`crate::SpeechController`].
///
/// Fails fast on the first violated invariant in the priority order
/// documented at the module level, so a profile with several
/// mistakes produces a deterministic single error.
///
/// ```
/// use heron_policy::{EscalationMode, PolicyProfile, validate};
///
/// // A profile author typo'd the destination — fail at session
/// // start, not on the first deny-topic escalation an hour later.
/// let bad = PolicyProfile {
///     allow_topics: vec![],
///     deny_topics: vec!["legal".into()],
///     mute: false,
///     escalation: EscalationMode::Notify { destination: "  ".into() },
/// };
/// assert!(validate(&bad).is_err());
/// ```
pub fn validate(profile: &PolicyProfile) -> Result<(), ValidationError> {
    for topic in &profile.allow_topics {
        if topic.trim().is_empty() {
            return Err(ValidationError::EmptyAllowTopic);
        }
    }
    for topic in &profile.deny_topics {
        if topic.trim().is_empty() {
            return Err(ValidationError::EmptyDenyTopic);
        }
    }

    if let Some(dup) = first_duplicate(&profile.allow_topics) {
        return Err(ValidationError::DuplicateAllowTopic { topic: dup });
    }
    if let Some(dup) = first_duplicate(&profile.deny_topics) {
        return Err(ValidationError::DuplicateDenyTopic { topic: dup });
    }

    // Deny wins at runtime per `filter::evaluate`, so an entry in
    // both lists is unreachable-as-allow. Almost always a typo;
    // refuse rather than quietly de-prioritize the allow entry.
    if let Some(overlap) = first_overlap(&profile.allow_topics, &profile.deny_topics) {
        return Err(ValidationError::AllowDenyOverlap { topic: overlap });
    }

    if let EscalationMode::Notify { destination } = &profile.escalation
        && destination.trim().is_empty()
    {
        return Err(ValidationError::EmptyNotifyDestination);
    }

    Ok(())
}

/// Return the first entry whose case-folded form has already
/// appeared earlier in `topics`. No `.trim()` — that would say two
/// strings are duplicates when `filter::evaluate` would treat them
/// as distinct matchers. Returns the original (un-normalized) string
/// of the *second* occurrence so the error message points at the
/// casing the author actually typed.
fn first_duplicate(topics: &[String]) -> Option<String> {
    let mut seen: Vec<String> = Vec::with_capacity(topics.len());
    for topic in topics {
        let key = topic.to_lowercase();
        if seen.iter().any(|s| s == &key) {
            return Some(topic.clone());
        }
        seen.push(key);
    }
    None
}

/// Return the first `allow` entry whose case-folded form also
/// appears in `deny`. No `.trim()`, mirroring `first_duplicate`.
/// Returns the allow-side original string so the error message names
/// what the author wrote.
fn first_overlap(allow: &[String], deny: &[String]) -> Option<String> {
    let deny_keys: Vec<String> = deny.iter().map(|t| t.to_lowercase()).collect();
    allow
        .iter()
        .find(|topic| {
            let key = topic.to_lowercase();
            deny_keys.iter().any(|d| d == &key)
        })
        .cloned()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn profile() -> PolicyProfile {
        PolicyProfile {
            allow_topics: vec![],
            deny_topics: vec![],
            mute: false,
            escalation: EscalationMode::None,
        }
    }

    #[test]
    fn default_profile_validates() {
        validate(&profile()).expect("default");
    }

    #[test]
    fn mute_true_still_validates() {
        // `mute` is the runtime kill switch honored by
        // `filter::evaluate`; setting it is a deployment decision,
        // not a configuration error.
        let mut p = profile();
        p.mute = true;
        validate(&p).expect("mute is a runtime concern, not a config error");
    }

    #[test]
    fn empty_allow_topic_rejected() {
        let mut p = profile();
        p.allow_topics = vec!["pricing".into(), "".into()];
        assert_eq!(validate(&p), Err(ValidationError::EmptyAllowTopic));
    }

    #[test]
    fn whitespace_only_allow_topic_rejected() {
        let mut p = profile();
        p.allow_topics = vec!["   ".into()];
        assert_eq!(validate(&p), Err(ValidationError::EmptyAllowTopic));
    }

    #[test]
    fn empty_deny_topic_rejected() {
        let mut p = profile();
        p.deny_topics = vec!["legal".into(), "".into()];
        assert_eq!(validate(&p), Err(ValidationError::EmptyDenyTopic));
    }

    #[test]
    fn duplicate_allow_case_insensitive_rejected() {
        // Substring matching in `filter::evaluate` is
        // case-insensitive, so `"Pricing"` and `"pricing"` are the
        // same topic — flag the second.
        let mut p = profile();
        p.allow_topics = vec!["Pricing".into(), "pricing".into()];
        match validate(&p).expect_err("duplicate") {
            ValidationError::DuplicateAllowTopic { topic } => assert_eq!(topic, "pricing"),
            other => panic!("expected DuplicateAllowTopic, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_padded_topic_is_not_a_duplicate() {
        // `"pricing"` and `"  Pricing  "` are NOT duplicates — at
        // runtime `filter::evaluate` does substring match without
        // trim, so the second entry only fires on utterances
        // literally containing whitespace-pricing-whitespace. The
        // validator must agree with the runtime; flagging this as
        // a duplicate would lie to the caller. Pin so a future
        // refactor that re-introduces `.trim()` in the dup key
        // surfaces here.
        let mut p = profile();
        p.allow_topics = vec!["pricing".into(), "  Pricing  ".into()];
        validate(&p).expect("whitespace-padded variant is a distinct matcher");
    }

    #[test]
    fn validator_and_runtime_agree_on_duplicates() {
        // The two strings the validator says are duplicates must
        // produce the same `filter::evaluate` decision on every
        // utterance — otherwise the validator is rejecting profiles
        // the runtime would accept. Mirror the case-insensitive,
        // no-trim normalization that `first_substring_match` uses.
        use crate::{PolicyDecision, evaluate};
        let dup_p = PolicyProfile {
            allow_topics: vec!["Pricing".into(), "pricing".into()],
            deny_topics: vec![],
            mute: false,
            escalation: EscalationMode::None,
        };
        // Validator rejects.
        assert!(matches!(
            validate(&dup_p),
            Err(ValidationError::DuplicateAllowTopic { .. })
        ));
        // Runtime: an utterance matching one matches the other.
        let single_p = PolicyProfile {
            allow_topics: vec!["pricing".into()],
            ..dup_p.clone()
        };
        assert!(matches!(
            evaluate("what's our pricing?", &single_p),
            PolicyDecision::Allowed
        ));
    }

    #[test]
    fn duplicate_deny_rejected() {
        let mut p = profile();
        p.deny_topics = vec!["legal".into(), "LEGAL".into()];
        match validate(&p).expect_err("duplicate") {
            ValidationError::DuplicateDenyTopic { topic } => assert_eq!(topic, "LEGAL"),
            other => panic!("expected DuplicateDenyTopic, got {other:?}"),
        }
    }

    #[test]
    fn allow_deny_overlap_rejected() {
        // Deny wins at runtime, so an allow entry that also appears
        // in deny is unreachable — refuse rather than silently
        // ignore the author's intent.
        let mut p = profile();
        p.allow_topics = vec!["pricing".into()];
        p.deny_topics = vec!["Pricing".into()];
        match validate(&p).expect_err("overlap") {
            ValidationError::AllowDenyOverlap { topic } => assert_eq!(topic, "pricing"),
            other => panic!("expected AllowDenyOverlap, got {other:?}"),
        }
    }

    #[test]
    fn empty_notify_destination_rejected() {
        let mut p = profile();
        p.escalation = EscalationMode::Notify {
            destination: "".into(),
        };
        assert_eq!(validate(&p), Err(ValidationError::EmptyNotifyDestination));
    }

    #[test]
    fn whitespace_notify_destination_rejected() {
        let mut p = profile();
        p.escalation = EscalationMode::Notify {
            destination: "   \t".into(),
        };
        assert_eq!(validate(&p), Err(ValidationError::EmptyNotifyDestination));
    }

    #[test]
    fn leave_meeting_escalation_validates() {
        // No destination field; the controller knows the action
        // intrinsically. Nothing to reject.
        let mut p = profile();
        p.escalation = EscalationMode::LeaveMeeting;
        validate(&p).expect("leave-meeting is self-describing");
    }

    #[test]
    fn realistic_profile_validates() {
        let p = PolicyProfile {
            allow_topics: vec!["pricing".into(), "launch dates".into()],
            deny_topics: vec!["compensation".into(), "legal".into()],
            mute: false,
            escalation: EscalationMode::Notify {
                destination: "slack:#sales".into(),
            },
        };
        validate(&p).expect("realistic profile");
    }

    #[test]
    fn priority_order_pin() {
        // Pin the priority order: allow-empty fires before
        // deny-empty so a profile with both errors produces the
        // same message run-to-run.
        let mut p = profile();
        p.allow_topics = vec!["".into()];
        p.deny_topics = vec!["".into()];
        assert_eq!(validate(&p), Err(ValidationError::EmptyAllowTopic));
    }
}
