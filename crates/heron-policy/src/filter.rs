//! Policy filter: decide whether a candidate utterance can leave
//! the agent's mouth.
//!
//! The [`crate::SpeechController`] consults [`evaluate`] before every
//! `speak()` emission with the rendered text + the active
//! [`PolicyProfile`]. The matcher is pure (no I/O, no clock) so
//! its decisions are reproducible from the audit log.
//!
//! ## Matching semantics
//!
//! Topics are case-insensitive substring matches. We deliberately
//! *don't* run a regex engine here: profile authors are users
//! configuring their bot, not security analysts authoring a WAF —
//! a substring match against `"compensation"` matches every
//! occurrence reliably without surprising users with regex
//! metacharacters.
//!
//! Rules in priority order (first match wins):
//!
//! 1. **`PolicyProfile::mute = true`** — every utterance returns
//!    [`PolicyDecision::Denied`] regardless of content.
//! 2. **deny_topics match** — utterance contains any deny term →
//!    [`PolicyDecision::Escalate`] if `EscalationMode::*` is set,
//!    [`PolicyDecision::Denied`] otherwise. Distinct outcomes so
//!    the controller knows whether to silently drop or to ping the
//!    user.
//! 3. **allow_topics empty** — no whitelist configured → always
//!    [`PolicyDecision::Allowed`] (open-by-default).
//! 4. **allow_topics non-empty + utterance matches one** → allowed.
//! 5. **allow_topics non-empty + no match** → denied with
//!    `"not_in_allow_list"` rule.
//!
//! The deny-before-allow priority is load-bearing: a profile with
//! `allow_topics: ["pricing"]` AND `deny_topics: ["legal"]` should
//! still escalate when the utterance contains both terms, because
//! "we'll send the legal contract along with our pricing" is
//! exactly the kind of overlap that needs human review.

use crate::{EscalationMode, PolicyProfile};

/// What the policy says about a candidate utterance. Distinct
/// `Denied` vs `Escalate` so the controller knows whether to
/// silently drop the utterance or to surface a notification to the
/// human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Utterance is fine to emit.
    Allowed,
    /// Hard mute or no-allow-match. Controller drops the utterance
    /// and (per spec) fires `SpeechEvent::Cancelled` with
    /// `CancelReason::PolicyDenied { rule }`.
    Denied { rule: String },
    /// The utterance touched a deny topic AND the profile
    /// configured an escalation. Controller drops the utterance,
    /// fires the same `Cancelled` event, AND triggers the
    /// configured [`EscalationMode`] (notify, leave meeting, etc.).
    Escalate { rule: String, via: EscalationMode },
}

impl PolicyDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, PolicyDecision::Allowed)
    }
}

/// Single matcher entry point. Pure / synchronous so the
/// controller can call it on the hot path of every TTS request
/// without paying for a thread hop.
pub fn evaluate(utterance: &str, profile: &PolicyProfile) -> PolicyDecision {
    if profile.mute {
        return PolicyDecision::Denied {
            rule: "muted".to_owned(),
        };
    }

    let lowered = utterance.to_lowercase();

    // Deny rules win against allow rules — see the load-bearing
    // example in the module doc-comment.
    if let Some(matched) = first_substring_match(&lowered, &profile.deny_topics) {
        return match &profile.escalation {
            EscalationMode::None => PolicyDecision::Denied {
                rule: format!("deny_topic:{matched}"),
            },
            EscalationMode::Notify { .. } | EscalationMode::LeaveMeeting => {
                PolicyDecision::Escalate {
                    rule: format!("deny_topic:{matched}"),
                    via: profile.escalation.clone(),
                }
            }
        };
    }

    // Empty allow list ⇒ open-by-default. A profile with no
    // explicit allow + no explicit deny means "say whatever you
    // want." Spec §9 design choice.
    if profile.allow_topics.is_empty() {
        return PolicyDecision::Allowed;
    }

    if first_substring_match(&lowered, &profile.allow_topics).is_some() {
        PolicyDecision::Allowed
    } else {
        PolicyDecision::Denied {
            rule: "not_in_allow_list".to_owned(),
        }
    }
}

/// Find the first topic in `needles` that appears as a substring
/// of `haystack` (already lower-cased by the caller). Returns the
/// matched needle so the rule string can name *which* topic fired.
fn first_substring_match<'a>(haystack: &str, needles: &'a [String]) -> Option<&'a str> {
    needles
        .iter()
        .map(String::as_str)
        .find(|needle| haystack.contains(&needle.to_lowercase()))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn profile(
        allow: &[&str],
        deny: &[&str],
        mute: bool,
        escalation: EscalationMode,
    ) -> PolicyProfile {
        PolicyProfile {
            allow_topics: allow.iter().map(|s| (*s).to_owned()).collect(),
            deny_topics: deny.iter().map(|s| (*s).to_owned()).collect(),
            mute,
            escalation,
        }
    }

    #[test]
    fn open_by_default_with_empty_lists_allows_anything() {
        let p = profile(&[], &[], false, EscalationMode::None);
        assert_eq!(evaluate("hello world", &p), PolicyDecision::Allowed);
    }

    #[test]
    fn mute_overrides_everything_else() {
        let p = profile(&["pricing"], &[], true, EscalationMode::None);
        match evaluate("let's talk pricing", &p) {
            PolicyDecision::Denied { rule } => assert_eq!(rule, "muted"),
            other => panic!("expected Denied(muted), got {other:?}"),
        }
    }

    #[test]
    fn deny_topic_without_escalation_denies() {
        let p = profile(&[], &["compensation"], false, EscalationMode::None);
        let decision = evaluate("their compensation package", &p);
        match decision {
            PolicyDecision::Denied { rule } => {
                assert!(rule.starts_with("deny_topic:"));
                assert!(rule.contains("compensation"));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn deny_topic_with_notify_escalates_with_via() {
        let p = profile(
            &[],
            &["legal"],
            false,
            EscalationMode::Notify {
                destination: "user@example.com".into(),
            },
        );
        let decision = evaluate("send the legal contract", &p);
        match decision {
            PolicyDecision::Escalate { rule, via } => {
                assert!(rule.contains("legal"));
                assert!(matches!(via, EscalationMode::Notify { .. }));
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn deny_topic_with_leave_meeting_escalates() {
        let p = profile(&[], &["pricing"], false, EscalationMode::LeaveMeeting);
        match evaluate("let's discuss pricing", &p) {
            PolicyDecision::Escalate { via, .. } => {
                assert!(matches!(via, EscalationMode::LeaveMeeting));
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn deny_wins_against_allow_when_both_match() {
        // Load-bearing per the module doc-comment: "send the legal
        // contract with the pricing" should escalate, not allow.
        let p = profile(
            &["pricing"],
            &["legal"],
            false,
            EscalationMode::Notify {
                destination: "x".into(),
            },
        );
        let decision = evaluate("send the legal contract with the pricing", &p);
        assert!(
            matches!(decision, PolicyDecision::Escalate { .. }),
            "deny must beat allow when both fire: {decision:?}"
        );
    }

    #[test]
    fn allow_list_blocks_unmatched_utterances() {
        let p = profile(&["pricing", "demo"], &[], false, EscalationMode::None);
        let decision = evaluate("the weather is great today", &p);
        match decision {
            PolicyDecision::Denied { rule } => assert_eq!(rule, "not_in_allow_list"),
            other => panic!("expected Denied(not_in_allow_list), got {other:?}"),
        }
    }

    #[test]
    fn allow_list_admits_substring_match() {
        let p = profile(&["pricing"], &[], false, EscalationMode::None);
        // "pricing" appears inside "Q3 pricing". Substring match
        // intentionally — strict word boundaries would frustrate
        // users who configure a topic and expect every form to hit.
        assert_eq!(
            evaluate("share the Q3 pricing", &p),
            PolicyDecision::Allowed
        );
    }

    #[test]
    fn case_insensitive_topic_match() {
        let p = profile(&[], &["LEGAL"], false, EscalationMode::None);
        match evaluate("the legal contract", &p) {
            PolicyDecision::Denied { rule } => assert!(rule.contains("LEGAL")),
            other => panic!("expected Denied, got {other:?}"),
        }
        // And the inverse — utterance UPPERCASE, topic lower.
        let p = profile(&[], &["legal"], false, EscalationMode::None);
        let decision = evaluate("THE LEGAL CONTRACT", &p);
        assert!(matches!(decision, PolicyDecision::Denied { .. }));
    }

    #[test]
    fn allow_list_match_is_case_insensitive() {
        let p = profile(&["Pricing"], &[], false, EscalationMode::None);
        assert_eq!(
            evaluate("PRICING is the topic", &p),
            PolicyDecision::Allowed
        );
    }

    #[test]
    fn empty_utterance_with_open_default_is_allowed() {
        let p = profile(&[], &[], false, EscalationMode::None);
        assert_eq!(evaluate("", &p), PolicyDecision::Allowed);
    }

    #[test]
    fn empty_utterance_with_allow_list_is_denied() {
        let p = profile(&["pricing"], &[], false, EscalationMode::None);
        match evaluate("", &p) {
            PolicyDecision::Denied { rule } => assert_eq!(rule, "not_in_allow_list"),
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn rule_string_names_the_specific_topic_that_fired() {
        // Catches a regression where the rule string just said
        // "deny_topic" without telling the user *which* term hit.
        let p = profile(
            &[],
            &["compensation", "termination", "litigation"],
            false,
            EscalationMode::None,
        );
        match evaluate("the litigation lawyer", &p) {
            PolicyDecision::Denied { rule } => {
                assert!(
                    rule.ends_with("litigation"),
                    "rule should name the matched topic: {rule}"
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn first_deny_match_wins_when_multiple_apply() {
        // Document the iteration order so a profile author can
        // predict which topic shows up in the rule string when
        // their utterance touches several.
        let p = profile(&[], &["legal", "pricing"], false, EscalationMode::None);
        match evaluate("legal pricing combo", &p) {
            PolicyDecision::Denied { rule } => {
                assert!(
                    rule.ends_with("legal"),
                    "first deny_topic entry should win: {rule}"
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn is_allowed_predicate_only_true_for_allowed() {
        assert!(PolicyDecision::Allowed.is_allowed());
        assert!(!PolicyDecision::Denied { rule: "x".into() }.is_allowed());
        assert!(
            !PolicyDecision::Escalate {
                rule: "x".into(),
                via: EscalationMode::None,
            }
            .is_allowed()
        );
    }

    #[test]
    fn deny_topic_with_escalation_none_falls_back_to_denied() {
        // Pin the contract: even though deny matched, if
        // EscalationMode is None, the decision is plain Denied
        // (not Escalate). Surfaces "the user explicitly said don't
        // ping me" as a distinct outcome from "the user told me
        // how to ping them."
        let p = profile(&[], &["legal"], false, EscalationMode::None);
        match evaluate("legal stuff", &p) {
            PolicyDecision::Denied { .. } => {}
            other => panic!("EscalationMode::None must produce Denied, got {other:?}"),
        }
    }
}
