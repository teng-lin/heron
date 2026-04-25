//! Disclosure-objection matcher per spec §4 + Invariant 6.
//!
//! After the bot speaks its disclosure, the orchestrator listens to
//! the inbound transcript stream for `objection_timeout_secs`
//! seconds. Any participant turn matching one of the configured
//! `objection_patterns` triggers the FSM's
//! [`crate::BotEvent::DisclosureObjected`] transition, which in turn
//! routes the bot to `Leaving → Completed` per
//! [`crate::BotFsm`].
//!
//! Why a separate module from `fsm.rs`:
//! - The FSM is event-shaped (consumes `BotEvent::DisclosureObjected`).
//!   The matcher is text-shaped (consumes inbound transcript turns
//!   and decides when to *produce* that event).
//! - Keeping the matcher pure (no clock, no I/O) lets the
//!   orchestrator unit-test "did this turn objection-match" without
//!   spinning up a real disclosure flow.
//!
//! ## Matching semantics
//!
//! Mirrors the policy filter: case-insensitive substring match. The
//! spec calls these "objection patterns" but a regex engine would
//! over-shoot — profile authors write things like `"please leave"` /
//! `"don't record"` / `"no AI"`, not regexes. A substring match
//! against those terms catches the common forms reliably without
//! the metacharacter footguns.
//!
//! ## What this returns
//!
//! [`match_objection`] returns the first matched pattern, so the
//! orchestrator can:
//!
//! 1. Log *which* pattern fired (for the audit log + the user-
//!    facing "exited because participant said X" indicator).
//! 2. Route the FSM to `DisclosureObjected` exactly once even if
//!    several inbound turns each match — the caller decides
//!    whether to ignore subsequent matches once the FSM has moved.

/// First pattern from `patterns` that appears as a substring of
/// `text` (case-insensitive). Returns `None` when no pattern fires
/// — the orchestrator keeps listening until the timeout expires.
pub fn match_objection<'a>(text: &str, patterns: &'a [String]) -> Option<&'a str> {
    if patterns.is_empty() || text.is_empty() {
        return None;
    }
    let lowered_text = text.to_lowercase();
    patterns
        .iter()
        .map(String::as_str)
        .find(|pattern| !pattern.is_empty() && lowered_text.contains(&pattern.to_lowercase()))
}

/// `true` when [`match_objection`] would fire. Convenience for the
/// orchestrator's hot loop: the boolean answer is what gates the
/// `BotEvent::DisclosureObjected` send.
pub fn is_objection(text: &str, patterns: &[String]) -> bool {
    match_objection(text, patterns).is_some()
}

/// Variables the [`DisclosureProfile::text_template`] can reference.
/// Spec §4 names two: `{user_name}` (the human heron is acting on
/// behalf of) and `{meeting_title}` (the calendar event the bot is
/// joining). Adding a new variable means adding a field here AND a
/// new `replace_placeholder` line — keeping the surface narrow on
/// purpose so a typo in the template surfaces as
/// [`TemplateError::UnknownPlaceholder`] rather than rendering an
/// empty string.
#[derive(Debug, Clone, Default)]
pub struct DisclosureVars<'a> {
    /// User the bot is acting on behalf of, e.g. `"Alex"`.
    pub user_name: &'a str,
    /// Calendar-derived meeting title, e.g. `"Acme Q3 review"`.
    pub meeting_title: &'a str,
}

/// Errors [`render_disclosure`] can return.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TemplateError {
    /// Template rendered to a whitespace-only string. Spec
    /// Invariant 6: a disclosure that the LLM is asked to speak
    /// must have actual content; "empty disclosure" is treated as
    /// "no disclosure" and rejected.
    #[error("template rendered to empty/whitespace-only output")]
    Empty,
    /// Template references a placeholder we don't know how to
    /// fill. Caller should fix the template (or extend
    /// [`DisclosureVars`] if a new variable is genuinely needed).
    #[error("template references unknown placeholder {{{name}}}")]
    UnknownPlaceholder { name: String },
}

/// Render `template` by replacing `{user_name}` / `{meeting_title}`
/// with `vars`. Unknown `{...}` placeholders surface as
/// [`TemplateError::UnknownPlaceholder`] so a template typo doesn't
/// silently leave a placeholder marker in the disclosure the bot
/// reads aloud.
///
/// Output gets `trim()`ed; an all-whitespace template returns
/// [`TemplateError::Empty`] so the caller fails fast rather than
/// emitting a silent / blank disclosure.
pub fn render_disclosure(
    template: &str,
    vars: &DisclosureVars<'_>,
) -> Result<String, TemplateError> {
    // Add 64 bytes of slack so a typical "{user_name}" → "Alex"
    // expansion (or a longer name) doesn't trigger a realloc on
    // the first push past the template length.
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        // Push the literal chunk before the `{`.
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        // `}` *after* the open — same line, no nesting allowed.
        // (We're a tiny renderer, not Handlebars.)
        let Some(close_rel) = after_open.find('}') else {
            // Unbalanced `{` with no matching `}`. Treat as a
            // literal `{` followed by the rest of the template,
            // matching how a casual template author would expect a
            // stray brace to be handled (verbatim).
            out.push('{');
            rest = after_open;
            continue;
        };
        let placeholder = &after_open[..close_rel];
        // A `{` inside the candidate placeholder means we crossed
        // a stray `{` rather than a real `{name}` pair — render
        // the outer `{` literal and rescan from there. Catches
        // typos like `"see {user_name and {user_name}"`.
        if placeholder.contains('{') {
            out.push('{');
            rest = after_open;
            continue;
        }
        match placeholder {
            "user_name" => out.push_str(vars.user_name),
            "meeting_title" => out.push_str(vars.meeting_title),
            other => {
                return Err(TemplateError::UnknownPlaceholder {
                    name: other.to_owned(),
                });
            }
        }
        rest = &after_open[close_rel + 1..];
    }
    // Trailing literal after the last `}`.
    out.push_str(rest);

    if out.trim().is_empty() {
        return Err(TemplateError::Empty);
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn patterns(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn finds_first_matching_pattern() {
        let pats = patterns(&["please leave", "no recording", "don't record"]);
        let result = match_objection("excuse me, no recording please", &pats);
        assert_eq!(result, Some("no recording"));
    }

    #[test]
    fn returns_first_pattern_when_multiple_match() {
        // Pin iteration order so audit logs are predictable: the
        // first pattern in the configured list wins.
        let pats = patterns(&["please leave", "no recording"]);
        let result = match_objection("please leave AND stop the recording", &pats);
        assert_eq!(result, Some("please leave"));
    }

    #[test]
    fn case_insensitive_in_both_directions() {
        let pats = patterns(&["Please Leave"]);
        assert_eq!(
            match_objection("you should PLEASE LEAVE now", &pats),
            Some("Please Leave"),
        );
        let pats = patterns(&["please leave"]);
        assert_eq!(match_objection("PLEASE LEAVE", &pats), Some("please leave"),);
    }

    #[test]
    fn empty_text_returns_none() {
        let pats = patterns(&["please leave"]);
        assert_eq!(match_objection("", &pats), None);
    }

    #[test]
    fn empty_pattern_list_returns_none() {
        let result = match_objection("please leave", &[]);
        assert_eq!(result, None);
    }

    #[test]
    fn empty_pattern_string_is_skipped() {
        // A misconfigured profile with `""` in the pattern list
        // would otherwise match every utterance (every string
        // contains the empty string). Skip empties so a typo can't
        // accidentally kick the bot out of every meeting.
        let pats = patterns(&["", "please leave"]);
        assert_eq!(
            match_objection("please leave the meeting", &pats),
            Some("please leave"),
        );
    }

    #[test]
    fn empty_pattern_string_only_returns_none() {
        let pats = patterns(&[""]);
        assert_eq!(match_objection("any text at all", &pats), None);
    }

    #[test]
    fn substring_match_does_not_require_word_boundary() {
        // "no" inside "innovation" intentionally matches — we don't
        // run a tokenizer here. The trade-off is documented in the
        // module preamble; profile authors should pick patterns
        // that aren't accidentally embedded in benign words.
        let pats = patterns(&["no"]);
        assert_eq!(match_objection("innovation", &pats), Some("no"));
    }

    #[test]
    fn is_objection_is_a_thin_wrapper() {
        let pats = patterns(&["please leave"]);
        assert!(is_objection("please leave now", &pats));
        assert!(!is_objection("good morning", &pats));
        assert!(!is_objection("anything", &[]));
    }

    #[test]
    fn realistic_objection_phrases() {
        // Sanity-check against the kind of phrasing real
        // participants would actually use.
        let pats = patterns(&[
            "please leave",
            "no recording",
            "don't record",
            "no AI",
            "stop the recording",
        ]);
        let cases = [
            ("Hey, please leave the call", Some("please leave")),
            ("we have a no recording policy here", Some("no recording")),
            ("Don't record this discussion", Some("don't record")),
            ("we're a no AI shop", Some("no AI")),
            ("STOP THE RECORDING right now", Some("stop the recording")),
            ("good morning everyone", None),
        ];
        for (text, expected) in cases {
            let result = match_objection(text, &pats);
            assert_eq!(
                result, expected,
                "failed on input: {text:?} expected {expected:?}, got {result:?}"
            );
        }
    }

    #[test]
    fn unicode_text_with_ascii_pattern() {
        // Mixed-encoding inputs: a Spanish "por favor déjenos" + an
        // English "please leave" pattern. The English pattern fires
        // only on English text (substring), so the Spanish doesn't
        // match. Pinned so we don't accidentally introduce a fancy
        // tokenizer that would.
        let pats = patterns(&["please leave"]);
        assert_eq!(match_objection("por favor déjenos", &pats), None);
        // Same English pattern, English-with-emoji input → matches.
        assert_eq!(
            match_objection("please leave 🙏", &pats),
            Some("please leave"),
        );
    }

    #[test]
    fn whitespace_only_text_returns_none() {
        // " " is not the empty string, but contains no objection
        // patterns either. Pin this so a transcript pause-marker
        // doesn't mistakenly fire the FSM.
        let pats = patterns(&["please leave"]);
        assert_eq!(match_objection("   \t\n  ", &pats), None);
    }

    fn vars<'a>(user: &'a str, title: &'a str) -> DisclosureVars<'a> {
        DisclosureVars {
            user_name: user,
            meeting_title: title,
        }
    }

    #[test]
    fn template_renders_user_name_and_meeting_title() {
        let template =
            "Hi, I'm an AI assistant joining on behalf of {user_name} for {meeting_title}.";
        let out = render_disclosure(template, &vars("Alex", "Acme Q3 review")).expect("render");
        assert_eq!(
            out,
            "Hi, I'm an AI assistant joining on behalf of Alex for Acme Q3 review."
        );
    }

    #[test]
    fn template_with_only_literals_passes_through() {
        let out = render_disclosure("Recording in progress.", &vars("", "")).expect("ok");
        assert_eq!(out, "Recording in progress.");
    }

    #[test]
    fn empty_template_errors() {
        let err = render_disclosure("", &vars("Alex", "x")).expect_err("empty");
        assert_eq!(err, TemplateError::Empty);
    }

    #[test]
    fn whitespace_only_template_errors() {
        let err = render_disclosure("   \n\t  ", &vars("Alex", "x")).expect_err("ws");
        assert_eq!(err, TemplateError::Empty);
    }

    #[test]
    fn template_with_only_empty_var_renders_to_empty_errors() {
        // {user_name} with empty var renders to "" — same outcome
        // as an empty template literal. Catches the case where a
        // caller forgot to populate vars before render.
        let err = render_disclosure("{user_name}", &vars("", "")).expect_err("empty");
        assert_eq!(err, TemplateError::Empty);
    }

    #[test]
    fn unknown_placeholder_errors_with_name() {
        let err = render_disclosure("hi {bogus}", &vars("a", "b")).expect_err("unknown");
        match err {
            TemplateError::UnknownPlaceholder { name } => assert_eq!(name, "bogus"),
            other => panic!("expected UnknownPlaceholder, got {other:?}"),
        }
    }

    #[test]
    fn placeholder_substring_of_unknown_var_is_caught() {
        // `{user_namex}` is NOT `{user_name}` — exact match only.
        // Pin so a future fuzzy-match optimization doesn't sneak in.
        let err = render_disclosure("{user_namex}", &vars("a", "b")).expect_err("unknown");
        assert!(matches!(
            err,
            TemplateError::UnknownPlaceholder { name } if name == "user_namex"
        ));
    }

    #[test]
    fn unbalanced_open_brace_treated_as_literal() {
        // A stray `{` with no matching `}` should render verbatim
        // rather than swallowing the rest of the template. Pin so
        // a typo like "see {user_name and you" still produces a
        // recognizable disclosure.
        let out = render_disclosure("see {user_name and {user_name}", &vars("Alex", ""))
            .expect("renders");
        // The first `{` is the unbalanced one; the second is the
        // legitimate placeholder. Output should include the literal
        // "{" and the substituted name.
        assert!(out.contains("{user_name and"), "got: {out}");
        assert!(out.ends_with("Alex"), "got: {out}");
    }

    #[test]
    fn multiple_placeholders_render_each() {
        let template = "{user_name} {user_name} for {meeting_title}";
        let out = render_disclosure(template, &vars("Alex", "Q3")).expect("ok");
        assert_eq!(out, "Alex Alex for Q3");
    }

    #[test]
    fn placeholder_with_special_characters_in_value() {
        // Multibyte + emoji + brace-in-value all round-trip
        // verbatim — no further substitution is applied to var
        // values, so a value of "{user_name}" doesn't recurse.
        let out = render_disclosure("{user_name}", &vars("Á́🦀{user_name}", "")).expect("ok");
        assert_eq!(out, "Á́🦀{user_name}");
    }

    #[test]
    fn render_is_deterministic_for_same_input() {
        let template = "{user_name} for {meeting_title}";
        let v = vars("Alex", "Q3");
        let a = render_disclosure(template, &v).expect("ok");
        let b = render_disclosure(template, &v).expect("ok");
        assert_eq!(a, b);
    }
}
